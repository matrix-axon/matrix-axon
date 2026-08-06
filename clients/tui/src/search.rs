use std::collections::HashMap;

use chrono::{Datelike, Duration, NaiveDate, TimeZone, Utc, Weekday};
use uuid::Uuid;

use crate::api::{EventDto, SearchResultDto};
use crate::command::{parse_date_to_ms, ymd_hm_to_ms};

pub(crate) const DEFAULT_SEARCH_LIMIT: usize = 20;
pub(crate) const DEFAULT_CONTEXT_RADIUS: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SearchCommandInput {
    OpenForm,
    Help,
    Run(String),
}

pub(crate) const SEARCH_HELP_TEXT: &str = "\
/search syntax

Bare /search opens the field form.
/search ? shows this help.
/search <query> searches the selected room.

Power syntax:
  /search [field:value...] <query>

Fields:
  room:<name|id>       search one room
  room:*               search all rooms in the current account
  account:<id|user|#>  limit to one account
  account:*            search all accounts
  all:true             alias for account:*
  sender:<text>        sender MXID substring
  from:<text>          synonym for sender:
  date:<date|range>    date or date range
  received:<date>      synonym for date:
  after:<date>         messages on/after date
  before:<date>        messages on/before date
  limit:<n>            result limit, 1-200

Dates accept mm/dd/yy, ranges like 5/1/26-5/31/26, and quoted human dates like \"last week\", \"this month\", or \"last Tuesday\".

Examples:
  /search backup
  /search room:* backup
  /search room:ops backup
  /search account:* backup
  /search account:alice room:* backup
  /search from:bob backup
  /search from:bob before:5/25/26";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedSearch {
    pub(crate) query: String,
    pub(crate) account: Option<String>,
    pub(crate) room: Option<String>,
    pub(crate) sender: Option<String>,
    pub(crate) all: bool,
    pub(crate) from: Option<i64>,
    pub(crate) to: Option<i64>,
    pub(crate) limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchRequest {
    pub(crate) q: String,
    pub(crate) account_id: Option<Uuid>,
    pub(crate) room_id: Option<String>,
    pub(crate) sender: Option<String>,
    pub(crate) from: Option<i64>,
    pub(crate) to: Option<i64>,
    pub(crate) limit: usize,
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SearchContextKey {
    pub(crate) account_id: Uuid,
    pub(crate) room_id: String,
    pub(crate) event_id: String,
}

impl SearchContextKey {
    pub(crate) fn from_event(event: &EventDto) -> Self {
        Self {
            account_id: event.account_id,
            room_id: event.room_id.clone(),
            event_id: event.event_id.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SearchResultContext {
    pub(crate) events: Vec<EventDto>,
}

#[derive(Debug, Clone)]
pub(crate) struct SearchResultsState {
    pub(crate) request: SearchRequest,
    pub(crate) edit_form: SearchFormState,
    pub(crate) results: Vec<SearchResultDto>,
    pub(crate) total: usize,
    pub(crate) next_cursor: Option<String>,
    pub(crate) selected: usize,
    pub(crate) loading: bool,
    pub(crate) sort_order: SearchSortOrder,
    pub(crate) grouping: SearchGrouping,
    pub(crate) context_cache: HashMap<SearchContextKey, SearchResultContext>,
}

impl SearchResultsState {
    pub(crate) fn select_first_ordered(&mut self) {
        self.selected = self.ordered_indices().first().copied().unwrap_or(0);
    }

    pub(crate) fn ordered_indices(&self) -> Vec<usize> {
        let mut indices = (0..self.results.len()).collect::<Vec<_>>();
        match self.grouping {
            SearchGrouping::None => {
                indices.sort_by(|&left, &right| self.compare_results_for_time_order(left, right));
            }
            SearchGrouping::Room => {
                let group_order = self.room_group_order();
                indices.sort_by(|&left, &right| {
                    let left_event = &self.results[left].event;
                    let right_event = &self.results[right].event;
                    group_order
                        .get(&(left_event.account_id, left_event.room_id.clone()))
                        .cmp(
                            &group_order
                                .get(&(right_event.account_id, right_event.room_id.clone())),
                        )
                        .then_with(|| self.compare_results_for_time_order(left, right))
                });
            }
        }
        indices
    }

    pub(crate) fn selected_order_position(&self) -> Option<usize> {
        self.ordered_indices()
            .iter()
            .position(|index| *index == self.selected)
    }

    fn compare_results_for_time_order(&self, left: usize, right: usize) -> std::cmp::Ordering {
        let left_event = &self.results[left].event;
        let right_event = &self.results[right].event;
        let by_time = match self.sort_order {
            SearchSortOrder::NewestFirst => right_event.origin_ts.cmp(&left_event.origin_ts),
            SearchSortOrder::OldestFirst => left_event.origin_ts.cmp(&right_event.origin_ts),
        };
        by_time
            .then_with(|| left_event.room_id.cmp(&right_event.room_id))
            .then_with(|| left_event.event_id.cmp(&right_event.event_id))
    }

    fn room_group_order(&self) -> HashMap<(Uuid, String), usize> {
        let mut room_bounds: HashMap<(Uuid, String), (i64, i64)> = HashMap::new();
        for result in &self.results {
            let event = &result.event;
            let entry = room_bounds
                .entry((event.account_id, event.room_id.clone()))
                .or_insert((event.origin_ts, event.origin_ts));
            entry.0 = entry.0.min(event.origin_ts);
            entry.1 = entry.1.max(event.origin_ts);
        }
        let mut rooms = room_bounds.into_iter().collect::<Vec<_>>();
        rooms.sort_by(
            |((left_account, left_room), (left_min, left_max)),
             ((right_account, right_room), (right_min, right_max))| {
                let by_time = match self.sort_order {
                    SearchSortOrder::NewestFirst => right_max.cmp(left_max),
                    SearchSortOrder::OldestFirst => left_min.cmp(right_min),
                };
                by_time
                    .then_with(|| left_room.cmp(right_room))
                    .then_with(|| left_account.cmp(right_account))
            },
        );
        rooms
            .into_iter()
            .enumerate()
            .map(|(order, (key, _))| (key, order))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchSortOrder {
    NewestFirst,
    OldestFirst,
}

impl SearchSortOrder {
    pub(crate) fn toggle(self) -> Self {
        match self {
            SearchSortOrder::NewestFirst => SearchSortOrder::OldestFirst,
            SearchSortOrder::OldestFirst => SearchSortOrder::NewestFirst,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            SearchSortOrder::NewestFirst => "newest first",
            SearchSortOrder::OldestFirst => "oldest first",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchGrouping {
    None,
    Room,
}

impl SearchGrouping {
    pub(crate) fn toggle(self) -> Self {
        match self {
            SearchGrouping::None => SearchGrouping::Room,
            SearchGrouping::Room => SearchGrouping::None,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            SearchGrouping::None => "time",
            SearchGrouping::Room => "room",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SearchFormField {
    Query,
    Scope,
    Account,
    Room,
    Sender,
    Date,
    After,
    Before,
    Limit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SearchScope {
    CurrentRoom,
    CurrentAccount,
    All,
    SpecificRoom,
    SpecificAccount,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchFormState {
    pub(crate) field: SearchFormField,
    pub(crate) scope: SearchScope,
    pub(crate) query: String,
    pub(crate) account: String,
    pub(crate) room: String,
    pub(crate) sender: String,
    pub(crate) date: String,
    pub(crate) after: String,
    pub(crate) before: String,
    pub(crate) limit: String,
    pub(crate) error: Option<String>,
}

impl Default for SearchFormState {
    fn default() -> Self {
        Self {
            field: SearchFormField::Query,
            scope: SearchScope::CurrentRoom,
            query: String::new(),
            account: String::new(),
            room: String::new(),
            sender: String::new(),
            date: String::new(),
            after: String::new(),
            before: String::new(),
            limit: DEFAULT_SEARCH_LIMIT.to_string(),
            error: None,
        }
    }
}

impl SearchFormState {
    pub(crate) fn from_parsed(parsed: &ParsedSearch) -> Self {
        let account = parsed.account.as_deref().unwrap_or_default();
        let room = parsed.room.as_deref().unwrap_or_default();
        let scope = if parsed.all || account == "*" {
            SearchScope::All
        } else if room == "*" {
            if account.is_empty() {
                SearchScope::CurrentAccount
            } else {
                SearchScope::SpecificAccount
            }
        } else if !room.is_empty() {
            SearchScope::SpecificRoom
        } else if !account.is_empty() {
            SearchScope::SpecificAccount
        } else {
            SearchScope::CurrentRoom
        };
        let (date, after, before) = date_fields_from_bounds(parsed.from, parsed.to);
        Self {
            field: SearchFormField::Query,
            scope,
            query: parsed.query.clone(),
            account: if account == "*" {
                String::new()
            } else {
                account.to_owned()
            },
            room: if room == "*" {
                String::new()
            } else {
                room.to_owned()
            },
            sender: parsed.sender.clone().unwrap_or_default(),
            date,
            after,
            before,
            limit: parsed.limit.to_string(),
            error: None,
        }
    }

    pub(crate) fn next_field(&mut self, reverse: bool) {
        let fields = self.visible_fields();
        let pos = fields
            .iter()
            .position(|field| *field == self.field)
            .unwrap_or(0);
        let next = if reverse {
            pos.checked_sub(1).unwrap_or(fields.len() - 1)
        } else {
            (pos + 1) % fields.len()
        };
        self.field = fields[next].clone();
    }

    pub(crate) fn cycle_scope(&mut self, reverse: bool) {
        use SearchScope::*;
        const SCOPES: [SearchScope; 5] = [
            CurrentRoom,
            CurrentAccount,
            All,
            SpecificRoom,
            SpecificAccount,
        ];
        let pos = SCOPES
            .iter()
            .position(|scope| *scope == self.scope)
            .unwrap_or(0);
        let next = if reverse {
            pos.checked_sub(1).unwrap_or(SCOPES.len() - 1)
        } else {
            (pos + 1) % SCOPES.len()
        };
        self.scope = SCOPES[next].clone();
        self.ensure_visible_field();
    }

    pub(crate) fn edit_buffer_mut(&mut self) -> Option<&mut String> {
        if !self.field_is_visible(&self.field) {
            self.ensure_visible_field();
        }
        match self.field {
            SearchFormField::Query => Some(&mut self.query),
            SearchFormField::Account => Some(&mut self.account),
            SearchFormField::Room => Some(&mut self.room),
            SearchFormField::Sender => Some(&mut self.sender),
            SearchFormField::Date => Some(&mut self.date),
            SearchFormField::After => Some(&mut self.after),
            SearchFormField::Before => Some(&mut self.before),
            SearchFormField::Limit => Some(&mut self.limit),
            SearchFormField::Scope => None,
        }
    }

    pub(crate) fn field_is_visible(&self, field: &SearchFormField) -> bool {
        self.visible_fields().iter().any(|visible| visible == field)
    }

    fn ensure_visible_field(&mut self) {
        if !self.field_is_visible(&self.field) {
            self.field = SearchFormField::Scope;
        }
    }

    fn visible_fields(&self) -> Vec<SearchFormField> {
        use SearchFormField::*;
        let mut fields = vec![Query, Scope];
        match self.scope {
            SearchScope::CurrentRoom | SearchScope::CurrentAccount | SearchScope::All => {}
            SearchScope::SpecificRoom => {
                fields.push(Room);
                fields.push(Account);
            }
            SearchScope::SpecificAccount => fields.push(Account),
        }
        fields.extend([Sender, Date, After, Before, Limit]);
        fields
    }

    pub(crate) fn to_parsed(&self) -> Result<ParsedSearch, String> {
        let mut tokens = Vec::new();
        match self.scope {
            SearchScope::CurrentRoom => {}
            SearchScope::CurrentAccount => tokens.push("room:*".to_owned()),
            SearchScope::All => tokens.push("account:*".to_owned()),
            SearchScope::SpecificRoom => {
                if self.room.trim().is_empty() {
                    return Err("specific room scope requires a room".to_owned());
                }
                tokens.push(format!("room:{}", quote_if_needed(&self.room)));
                if !self.account.trim().is_empty() {
                    tokens.push(format!("account:{}", quote_if_needed(&self.account)));
                }
            }
            SearchScope::SpecificAccount => {
                if self.account.trim().is_empty() {
                    return Err("specific account scope requires an account".to_owned());
                }
                tokens.push(format!("account:{}", quote_if_needed(&self.account)));
                tokens.push("room:*".to_owned());
            }
        }
        if !self.sender.trim().is_empty() {
            tokens.push(format!("sender:{}", quote_if_needed(&self.sender)));
        }
        if !self.date.trim().is_empty() {
            tokens.push(format!("date:{}", quote_if_needed(&self.date)));
        }
        if !self.after.trim().is_empty() {
            tokens.push(format!("after:{}", quote_if_needed(&self.after)));
        }
        if !self.before.trim().is_empty() {
            tokens.push(format!("before:{}", quote_if_needed(&self.before)));
        }
        if !self.limit.trim().is_empty() {
            tokens.push(format!("limit:{}", self.limit.trim()));
        }
        tokens.push(self.query.clone());
        parse_search_terms(&tokens.join(" "))
    }
}

fn date_fields_from_bounds(from: Option<i64>, to: Option<i64>) -> (String, String, String) {
    match (from.and_then(utc_day_from_ms), to.and_then(utc_day_from_ms)) {
        (Some(start), Some(end)) if start == end => {
            (format_form_date(start), String::new(), String::new())
        }
        (Some(start), Some(end)) => (
            format!("{}-{}", format_form_date(start), format_form_date(end)),
            String::new(),
            String::new(),
        ),
        (Some(start), None) => (String::new(), format_form_date(start), String::new()),
        (None, Some(end)) => (String::new(), String::new(), format_form_date(end)),
        (None, None) => (String::new(), String::new(), String::new()),
    }
}

fn utc_day_from_ms(ms: i64) -> Option<NaiveDate> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.date_naive())
}

fn format_form_date(day: NaiveDate) -> String {
    format!("{}/{}/{}", day.month(), day.day(), day.year() % 100)
}

fn quote_if_needed(value: &str) -> String {
    let value = value.trim();
    if value.chars().any(char::is_whitespace) {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

pub(crate) fn parse_search_command_arg(arg: &str) -> SearchCommandInput {
    let arg = arg.trim();
    if arg.is_empty() {
        SearchCommandInput::OpenForm
    } else if arg == "?" {
        SearchCommandInput::Help
    } else {
        SearchCommandInput::Run(arg.to_owned())
    }
}

pub(crate) fn parse_search_terms(input: &str) -> Result<ParsedSearch, String> {
    let tokens = shell_words(input)?;
    let mut parsed = ParsedSearch {
        query: String::new(),
        account: None,
        room: None,
        sender: None,
        all: false,
        from: None,
        to: None,
        limit: DEFAULT_SEARCH_LIMIT,
    };
    let mut parsing_fields = true;
    let mut query_start = None;
    for token in tokens {
        if token.cooked == "--" {
            parsing_fields = false;
            query_start = token.end.checked_add(1);
            continue;
        }
        if parsing_fields {
            if let Some((field, value)) = token.cooked.split_once(':') {
                match field {
                    "account" => {
                        parsed.account = Some(nonempty_value(field, value)?.to_owned());
                        continue;
                    }
                    "room" => {
                        parsed.room = Some(nonempty_value(field, value)?.to_owned());
                        continue;
                    }
                    "sender" | "from" => {
                        parsed.sender = Some(nonempty_value(field, value)?.to_owned());
                        continue;
                    }
                    "all" => {
                        parsed.all = parse_bool_value(field, value)?;
                        continue;
                    }
                    "limit" => {
                        parsed.limit = nonempty_value(field, value)?
                            .parse::<usize>()
                            .map_err(|_| "limit must be a number".to_owned())?
                            .clamp(1, 200);
                        continue;
                    }
                    "date" | "received" => {
                        apply_date_range(&mut parsed, value)?;
                        continue;
                    }
                    "before" | "to" => {
                        parsed.to = Some(day_range(value)?.1);
                        continue;
                    }
                    "after" => {
                        parsed.from = Some(day_range(value)?.0);
                        continue;
                    }
                    _ => {}
                }
            }
        }
        query_start = Some(token.start);
        break;
    }
    parsed.query = query_start
        .and_then(|start| input.get(start..))
        .unwrap_or_default()
        .trim()
        .to_owned();
    if parsed.all && (parsed.account.is_some() || parsed.room.is_some()) {
        return Err("all:true cannot be combined with account: or room:".to_owned());
    }
    if let (Some(from), Some(to)) = (parsed.from, parsed.to) {
        if from > to {
            return Err("date range starts after it ends".to_owned());
        }
    }
    Ok(parsed)
}

fn nonempty_value<'a>(field: &str, value: &'a str) -> Result<&'a str, String> {
    let value = value.trim();
    if value.is_empty() {
        Err(format!("{field}: requires a value"))
    } else {
        Ok(value)
    }
}

fn parse_bool_value(field: &str, value: &str) -> Result<bool, String> {
    match value.trim() {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(format!("{field}: must be true or false")),
    }
}

fn apply_date_range(parsed: &mut ParsedSearch, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("date: requires a value".to_owned());
    }
    if let Some((start, end)) = split_date_range(value) {
        if !start.trim().is_empty() {
            parsed.from = Some(day_range(start)?.0);
        }
        if !end.trim().is_empty() {
            parsed.to = Some(day_range(end)?.1);
        }
        if start.trim().is_empty() && end.trim().is_empty() {
            return Err("date range requires a start or end date".to_owned());
        }
    } else {
        let (from, to) = day_range(value)?;
        parsed.from = Some(from);
        parsed.to = Some(to);
    }
    Ok(())
}

fn split_date_range(value: &str) -> Option<(&str, &str)> {
    if let Some(end) = value.strip_prefix('-') {
        return Some(("", end));
    }
    for (idx, ch) in value.char_indices() {
        if ch != '-' {
            continue;
        }
        let start = &value[..idx];
        let end = &value[idx + ch.len_utf8()..];
        let start_ok = start.trim().is_empty() || day_range(start).is_ok();
        let end_ok = end.trim().is_empty() || day_range(end).is_ok();
        if start_ok && end_ok && !(start.trim().is_empty() && end.trim().is_empty()) {
            return Some((start, end));
        }
    }
    None
}

fn day_range(value: &str) -> Result<(i64, i64), String> {
    let value = value.trim();
    let (start, end) = human_date_range(value)
        .or_else(|| parse_single_day(value).map(|d| (d, d)))
        .ok_or_else(|| format!("unrecognized date: {value}"))?;
    Ok((start_of_day_ms(start), end_of_day_ms(end)))
}

fn human_date_range(value: &str) -> Option<(NaiveDate, NaiveDate)> {
    let today = Utc::now().date_naive();
    match value.to_ascii_lowercase().as_str() {
        "last week" => {
            let start_this_week =
                today - Duration::days(today.weekday().num_days_from_monday() as i64);
            let start = start_this_week - Duration::days(7);
            Some((start, start + Duration::days(6)))
        }
        "this week" => {
            let start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
            Some((start, start + Duration::days(6)))
        }
        "last month" => {
            let first_this = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)?;
            let last_prev = first_this - Duration::days(1);
            let first_prev = NaiveDate::from_ymd_opt(last_prev.year(), last_prev.month(), 1)?;
            Some((first_prev, last_prev))
        }
        "this month" => {
            let start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)?;
            let end = if today.month() == 12 {
                NaiveDate::from_ymd_opt(today.year() + 1, 1, 1)?
            } else {
                NaiveDate::from_ymd_opt(today.year(), today.month() + 1, 1)?
            } - Duration::days(1);
            Some((start, end))
        }
        "last year" => {
            let year = today.year() - 1;
            Some((
                NaiveDate::from_ymd_opt(year, 1, 1)?,
                NaiveDate::from_ymd_opt(year, 12, 31)?,
            ))
        }
        "this year" => Some((
            NaiveDate::from_ymd_opt(today.year(), 1, 1)?,
            NaiveDate::from_ymd_opt(today.year(), 12, 31)?,
        )),
        other if other.starts_with("last ") => {
            let weekday = parse_weekday(other.strip_prefix("last ")?)?;
            let mut date = today - Duration::days(1);
            while date.weekday() != weekday {
                date -= Duration::days(1);
            }
            Some((date, date))
        }
        _ => None,
    }
}

fn parse_weekday(value: &str) -> Option<Weekday> {
    match value {
        "monday" | "mon" => Some(Weekday::Mon),
        "tuesday" | "tue" | "tues" => Some(Weekday::Tue),
        "wednesday" | "wed" => Some(Weekday::Wed),
        "thursday" | "thu" | "thur" | "thurs" => Some(Weekday::Thu),
        "friday" | "fri" => Some(Weekday::Fri),
        "saturday" | "sat" => Some(Weekday::Sat),
        "sunday" | "sun" => Some(Weekday::Sun),
        _ => None,
    }
}

fn parse_single_day(value: &str) -> Option<NaiveDate> {
    let parts: Vec<&str> = value.split('/').collect();
    if parts.len() == 3 {
        let month = parts[0].parse::<u32>().ok()?;
        let day = parts[1].parse::<u32>().ok()?;
        let mut year = parts[2].parse::<i32>().ok()?;
        if (0..=99).contains(&year) {
            year += 2000;
        }
        return NaiveDate::from_ymd_opt(year, month, day);
    }
    parse_date_to_ms(value).and_then(utc_day_from_ms)
}

fn start_of_day_ms(day: NaiveDate) -> i64 {
    ymd_hm_to_ms(day.year() as i64, day.month(), day.day(), 0, 0).expect("valid UTC date")
}

fn end_of_day_ms(day: NaiveDate) -> i64 {
    let next_day = day + Duration::days(1);
    start_of_day_ms(next_day) - 1
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellWord {
    cooked: String,
    start: usize,
    end: usize,
}

fn shell_words(input: &str) -> Result<Vec<ShellWord>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut start = None;
    let mut end = 0;
    let mut in_quote = false;
    let mut chars = input.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        match ch {
            '"' => {
                start.get_or_insert(idx);
                in_quote = !in_quote;
                end = idx + ch.len_utf8();
            }
            '\\' if in_quote => {
                start.get_or_insert(idx);
                if let Some((next_idx, next)) = chars.next() {
                    current.push(next);
                    end = next_idx + next.len_utf8();
                }
            }
            ch if ch.is_whitespace() && !in_quote => {
                if let Some(word_start) = start.take() {
                    words.push(ShellWord {
                        cooked: std::mem::take(&mut current),
                        start: word_start,
                        end,
                    });
                }
            }
            _ => {
                start.get_or_insert(idx);
                current.push(ch);
                end = idx + ch.len_utf8();
            }
        }
    }
    if in_quote {
        return Err("unterminated quote in /search".to_owned());
    }
    if let Some(word_start) = start {
        words.push(ShellWord {
            cooked: current,
            start: word_start,
            end,
        });
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search_result(room_id: &str, event_id: &str, origin_ts: i64) -> SearchResultDto {
        SearchResultDto {
            event: EventDto {
                account_id: Uuid::nil(),
                event_id: event_id.to_owned(),
                room_id: room_id.to_owned(),
                sender: "@alice:example.org".to_owned(),
                state_key: None,
                arrival_order: origin_ts,
                origin_ts,
                event_type: "m.room.message".to_owned(),
                content: None,
                body: Some(event_id.to_owned()),
                relates_to: None,
                redacted: false,
                redaction_event_id: None,
                reactions: None,
                sender_trust: None,
            },
            score: 1.0,
        }
    }

    fn search_state(results: Vec<SearchResultDto>) -> SearchResultsState {
        SearchResultsState {
            request: SearchRequest {
                q: "needle".to_owned(),
                account_id: None,
                room_id: None,
                sender: None,
                from: None,
                to: None,
                limit: DEFAULT_SEARCH_LIMIT,
                cursor: None,
            },
            edit_form: SearchFormState::from_parsed(&parse_search_terms("needle").unwrap()),
            results,
            total: 0,
            next_cursor: None,
            selected: 0,
            loading: false,
            sort_order: SearchSortOrder::NewestFirst,
            grouping: SearchGrouping::None,
            context_cache: HashMap::new(),
        }
    }

    #[test]
    fn orders_loaded_results_by_selected_time_order() {
        let mut state = search_state(vec![
            search_result("!a:example.org", "$old", 10),
            search_result("!b:example.org", "$new", 30),
        ]);
        assert_eq!(state.ordered_indices(), vec![1, 0]);

        state.sort_order = SearchSortOrder::OldestFirst;
        assert_eq!(state.ordered_indices(), vec![0, 1]);
    }

    #[test]
    fn groups_loaded_results_by_room_then_time() {
        let mut state = search_state(vec![
            search_result("!a:example.org", "$a-old", 10),
            search_result("!b:example.org", "$b", 20),
            search_result("!a:example.org", "$a-new", 30),
        ]);
        state.grouping = SearchGrouping::Room;
        assert_eq!(state.ordered_indices(), vec![2, 0, 1]);

        state.sort_order = SearchSortOrder::OldestFirst;
        assert_eq!(state.ordered_indices(), vec![0, 2, 1]);
    }

    #[test]
    fn selects_first_result_in_current_order() {
        let mut state = search_state(vec![
            search_result("!a:example.org", "$old", 10),
            search_result("!b:example.org", "$new", 30),
        ]);
        state.selected = 0;

        state.select_first_ordered();
        assert_eq!(state.selected, 1);

        state.sort_order = SearchSortOrder::OldestFirst;
        state.select_first_ordered();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn selects_first_grouped_result_in_current_order() {
        let mut state = search_state(vec![
            search_result("!a:example.org", "$a-old", 10),
            search_result("!b:example.org", "$b", 20),
            search_result("!a:example.org", "$a-new", 30),
        ]);
        state.selected = 1;
        state.grouping = SearchGrouping::Room;

        state.select_first_ordered();
        assert_eq!(state.selected, 2);
    }

    #[test]
    fn search_form_skips_target_fields_for_implicit_scopes() {
        let mut form = SearchFormState::default();

        form.next_field(false);
        assert_eq!(form.field, SearchFormField::Scope);
        form.next_field(false);
        assert_eq!(form.field, SearchFormField::Sender);
    }

    #[test]
    fn search_form_shows_room_and_optional_account_for_specific_room() {
        let mut form = SearchFormState {
            scope: SearchScope::SpecificRoom,
            ..Default::default()
        };

        form.next_field(false);
        assert_eq!(form.field, SearchFormField::Scope);
        form.next_field(false);
        assert_eq!(form.field, SearchFormField::Room);
        form.next_field(false);
        assert_eq!(form.field, SearchFormField::Account);
    }

    #[test]
    fn search_form_shows_account_for_specific_account() {
        let mut form = SearchFormState {
            scope: SearchScope::SpecificAccount,
            ..Default::default()
        };

        form.next_field(false);
        assert_eq!(form.field, SearchFormField::Scope);
        form.next_field(false);
        assert_eq!(form.field, SearchFormField::Account);
        form.next_field(false);
        assert_eq!(form.field, SearchFormField::Sender);
    }

    #[test]
    fn search_form_specific_account_targets_all_rooms_in_account() {
        let form = SearchFormState {
            scope: SearchScope::SpecificAccount,
            query: "needle".to_owned(),
            account: "@alice:example.org".to_owned(),
            ..Default::default()
        };

        let parsed = form.to_parsed().unwrap();
        assert_eq!(parsed.account.as_deref(), Some("@alice:example.org"));
        assert_eq!(parsed.room.as_deref(), Some("*"));
        assert!(!parsed.all);
    }

    #[test]
    fn search_form_specific_room_allows_account_disambiguator() {
        let form = SearchFormState {
            scope: SearchScope::SpecificRoom,
            query: "needle".to_owned(),
            room: "general".to_owned(),
            account: "@alice:example.org".to_owned(),
            ..Default::default()
        };

        let parsed = form.to_parsed().unwrap();
        assert_eq!(parsed.room.as_deref(), Some("general"));
        assert_eq!(parsed.account.as_deref(), Some("@alice:example.org"));
        assert!(!parsed.all);
    }

    #[test]
    fn search_form_from_parsed_restores_editable_fields() {
        let parsed = parse_search_terms(
            "account:alice room:* sender:@bob:example.org date:5/1/26-5/3/26 limit:50 backup key",
        )
        .unwrap();

        let form = SearchFormState::from_parsed(&parsed);

        assert_eq!(form.scope, SearchScope::SpecificAccount);
        assert_eq!(form.query, "backup key");
        assert_eq!(form.account, "alice");
        assert_eq!(form.sender, "@bob:example.org");
        assert_eq!(form.date, "5/1/26-5/3/26");
        assert_eq!(form.limit, "50");
    }

    #[test]
    fn search_form_from_parsed_restores_all_accounts_scope() {
        let parsed = parse_search_terms("account:* backup").unwrap();

        let form = SearchFormState::from_parsed(&parsed);

        assert_eq!(form.scope, SearchScope::All);
        assert_eq!(form.query, "backup");
        assert_eq!(form.account, "");
    }

    #[test]
    fn parses_plain_query() {
        let parsed = parse_search_terms("hello world").unwrap();
        assert_eq!(parsed.query, "hello world");
        assert_eq!(parsed.limit, DEFAULT_SEARCH_LIMIT);
    }

    #[test]
    fn parses_filters_and_preserves_query_quotes() {
        let parsed =
            parse_search_terms("room:general sender:@a:b date:5/1/26-5/31/26 \"exact words\"")
                .unwrap();
        assert_eq!(parsed.room.as_deref(), Some("general"));
        assert_eq!(parsed.sender.as_deref(), Some("@a:b"));
        assert_eq!(parsed.query, "\"exact words\"");
        assert!(parsed.from.is_some());
        assert!(parsed.to.is_some());
    }

    #[test]
    fn parses_from_as_sender_alias() {
        let parsed = parse_search_terms("from:@bob:example.org backup").unwrap();
        assert_eq!(parsed.sender.as_deref(), Some("@bob:example.org"));
        assert_eq!(parsed.query, "backup");
        assert!(parsed.from.is_none());
    }

    #[test]
    fn parses_sender_filter_without_query() {
        let parsed = parse_search_terms("from:bob").unwrap();
        assert_eq!(parsed.sender.as_deref(), Some("bob"));
        assert_eq!(parsed.query, "");
    }

    #[test]
    fn parses_date_filter_without_query() {
        let parsed = parse_search_terms("before:5/25/26").unwrap();
        assert!(parsed.to.is_some());
        assert_eq!(parsed.query, "");
    }

    #[test]
    fn room_filter_requires_value() {
        let err = parse_search_terms("room: needle").unwrap_err();
        assert_eq!(err, "room: requires a value");
    }

    #[test]
    fn parses_single_iso_date_as_one_utc_day() {
        let parsed = parse_search_terms("date:2026-05-25 backup").unwrap();
        assert_eq!(parsed.from, Some(1_779_667_200_000));
        assert_eq!(parsed.to, Some(1_779_753_599_999));
        assert_eq!(parsed.query, "backup");
    }

    #[test]
    fn parses_iso_date_range_separator_between_dates() {
        let parsed = parse_search_terms("date:2026-05-25-2026-05-26").unwrap();
        assert_eq!(parsed.from, Some(1_779_667_200_000));
        assert_eq!(parsed.to, Some(1_779_839_999_999));
    }

    #[test]
    fn parses_empty_filterless_terms_for_request_level_validation() {
        let parsed = parse_search_terms("").unwrap();
        assert_eq!(parsed.query, "");
        assert_eq!(parsed.account, None);
        assert_eq!(parsed.room, None);
        assert_eq!(parsed.sender, None);
        assert!(parsed.from.is_none());
        assert!(parsed.to.is_none());
    }

    #[test]
    fn parses_wildcard_room_and_account_filters() {
        let room = parse_search_terms("room:* backup").unwrap();
        assert_eq!(room.room.as_deref(), Some("*"));
        assert_eq!(room.account, None);
        assert!(!room.all);

        let account = parse_search_terms("account:* backup").unwrap();
        assert_eq!(account.account.as_deref(), Some("*"));
        assert_eq!(account.room, None);
        assert!(!account.all);

        let old_all_alias = parse_search_terms("all:true backup").unwrap();
        assert!(old_all_alias.all);
    }

    #[test]
    fn parses_received_as_date_synonym_and_open_ranges() {
        let received = parse_search_terms("received:-5/25/26 key").unwrap();
        assert!(received.from.is_none());
        assert!(received.to.is_some());
        let after = parse_search_terms("date:5/25/26- key").unwrap();
        assert!(after.from.is_some());
        assert!(after.to.is_none());
    }

    #[test]
    fn parses_quoted_human_dates() {
        let parsed = parse_search_terms("after:\"last Tuesday\" backup").unwrap();
        assert_eq!(parsed.query, "backup");
        assert!(parsed.from.is_some());
    }

    #[test]
    fn parses_named_month_dates_like_jump() {
        let parsed = parse_search_terms("date:\"Jan 15 2025\" backup").unwrap();
        assert_eq!(parsed.from, Some(1_736_899_200_000));
        assert_eq!(parsed.to, Some(1_736_985_599_999));
        assert_eq!(parsed.query, "backup");
    }
}
