use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use serde::Deserialize;
use thiserror::Error;
use toml_edit::{Array, DocumentMut, Item, Value};

const INVALID_OPTION_COMMENT: &str =
    "# Commented out because this is not a valid axon-tui config option.";

pub const DEFAULT_CONFIG: &str = r#"# axon-tui configuration.
#
# Key names are case-insensitive. Supported forms include:
#   ctrl-n, ctrl-p, ctrl-c, ctrl-j, ctrl-k, f6, ctrl-f6, tab, enter, esc,
#   backspace, ctrl-a, ctrl-e, home, end, up, down, left, right,
#   pageup, pagedown, space, r, t, shift-r
#
# Color names are case-insensitive. Supported names:
#   black, red, green, yellow, blue, magenta, cyan, gray,
#   dark-gray, light-red, light-green, light-yellow, light-blue,
#   light-magenta, light-cyan, white, default

[shortcuts]
next_room = "ctrl-n"
previous_room = "ctrl-p"
next_account = "alt-n"
previous_account = "alt-p"
quit = "ctrl-c"
complete = "tab"
submit = "enter"
clear_input = "esc"
backspace = "backspace"
cursor_start = "ctrl-a"
cursor_end = "ctrl-e"
cursor_left = "left"
cursor_right = "right"
message_down = "ctrl-j"
message_up = "ctrl-k"
message_page_up = "pageup"
message_page_down = "pagedown"
edit_previous = "up"
edit_next = "down"
media_preview = "v"
newline = "alt-enter"
reply = "r"
thread = "t"
search_sort = "s"
search_group = "g"
search_edit = "e"
edit_message = "e"
redact_message = "d"
react_message = "shift-r"
unreact_message = "shift-u"
unread_threads = "alt-t"
focus_next = "f6"
focus_prev = "ctrl-f6"
find = "ctrl-f"
toggle_accounts_panel = "alt-a"
toggle_rooms_panel = "alt-r"
toggle_unread_filter = "alt-u"
refresh = "ctrl-l"
pin_room = "p"
unpin_room = "shift-p"
jump_day_back = "shift-pageup"
jump_day_forward = "shift-pagedown"
room_filter_cycle = "alt-f"
room_sort_cycle = "alt-s"
room_filter_dms = "alt-d"
room_filter_groups = "alt-g"
room_filter_favorites = "alt-v"
room_filter_all = "alt-0"
room_filter_by_name = "alt-/"
room_sort_recent = "alt-1"
room_sort_alpha = "alt-2"

# Select one theme by leaving its lines uncommented; comment out all others.
# Named foreground colors: border, selected_room, unread_count, message_sender,
#   own_message_sender, input_hint, status
# Selection background used when display.highlight_selected_line is enabled:
#   selection_background
# Per-pane base foreground (uncolored text) and background:
#   accounts_foreground, rooms_foreground, messages_foreground, input_foreground
#   accounts_background, rooms_background, messages_background, input_background
# Popup background: popup_background
# Global background: background
# All colors accept: black, red, green, yellow, blue, magenta, cyan, gray,
#   dark-gray, light-red, light-green, light-yellow, light-blue, light-magenta,
#   light-cyan, white, default
[colors]
# ── Default ───────────────────────────────────────────────────────────────────
border = "gray"
selected_room = "cyan"
unread_count = "yellow"
message_sender = "green"
own_message_sender = "light-cyan"
input_hint = "dark-gray"
status = "cyan"
background = "default"
selection_background = "dark-gray"

# ── Dracula ───────────────────────────────────────────────────────────────────
# border = "light-magenta"
# selected_room = "light-magenta"
# unread_count = "light-yellow"
# message_sender = "light-cyan"
# own_message_sender = "magenta"
# input_hint = "dark-gray"
# status = "light-magenta"
# background = "black"
# popup_background = "dark-gray"

# ── Nord ──────────────────────────────────────────────────────────────────────
# border = "light-blue"
# selected_room = "light-blue"
# unread_count = "light-yellow"
# message_sender = "light-cyan"
# own_message_sender = "cyan"
# input_hint = "gray"
# status = "light-blue"
# background = "dark-gray"
# accounts_foreground = "white"
# rooms_foreground = "white"
# accounts_background = "black"
# rooms_background = "black"
# popup_background = "dark-gray"

# ── Solarized Dark ────────────────────────────────────────────────────────────
# border = "yellow"
# selected_room = "light-green"
# unread_count = "light-red"
# message_sender = "light-cyan"
# own_message_sender = "cyan"
# input_hint = "dark-gray"
# status = "light-green"
# background = "black"
# popup_background = "dark-gray"

# ── Paper (Light) ─────────────────────────────────────────────────────────────
# border = "gray"
# selected_room = "blue"
# unread_count = "red"
# message_sender = "blue"
# own_message_sender = "dark-gray"
# input_hint = "gray"
# status = "blue"
# background = "white"
# accounts_foreground = "black"
# rooms_foreground = "black"
# messages_foreground = "black"
# input_foreground = "black"
# accounts_background = "gray"
# rooms_background = "gray"
# popup_background = "gray"

[display]
debug = false
show_state_events = false
message_density = "normal"
time_format = "24h"
input_lines = 1
max_input_lines = 10
# preview_warmup_count = 5
highlight_selected_line = false
confirm_logout = true
search_wrap = true
room_sort = "recent"
room_filter = "all"
accept_incoming_verification = true

# ── Server connection ─────────────────────────────────────────────────────────
# Uncomment and set these if your Axon server is not on the default address
# (http://127.0.0.1:8080) or requires bearer-token authentication.
#
# [server]
# base_url = "https://axon.example.com"
# bearer_token = "<token>"
"#;

pub const DEFAULT_ACCOUNTS_PANEL_WIDTH: u16 = 25;

#[derive(Debug, Clone)]
pub struct TuiConfig {
    pub shortcuts: Shortcuts,
    pub colors: ColorScheme,
    pub display: DisplayOptions,
    pub path: PathBuf,
    pub created_default: bool,
    pub base_url: Option<String>,
    pub token: Option<String>,
}

impl TuiConfig {
    pub fn load_or_create_default() -> Result<Self, ConfigError> {
        let path = config_path()?;
        Self::load_or_create_at(path)
    }

    pub fn load_or_create_at(path: PathBuf) -> Result<Self, ConfigError> {
        let created_default = ensure_default_config(&path)?;
        let text = fs::read_to_string(&path)?;
        let (raw, needs_repair) = RawConfig::load_with_defaults(&text)?;
        let (preserved, commented_invalid) = comment_invalid_options(&text);
        if needs_repair || commented_invalid {
            fs::write(&path, raw.rewrite_preserving(&preserved, false)?)?;
        }
        Ok(Self {
            shortcuts: raw.shortcuts.into_shortcuts()?,
            colors: raw.colors.into_color_scheme()?,
            display: raw.display.into_display_options()?,
            path,
            created_default,
            base_url: raw.base_url,
            token: raw.token,
        })
    }

    /// Persist the current display options back to the config file at `path`.
    /// Only the display section is updated; all other settings are preserved as-is.
    pub fn save_display(
        path: &std::path::Path,
        input_lines: u16,
        accounts_panel_width: u16,
        rooms_panel_width_adj: i16,
    ) -> Result<(), ConfigError> {
        let text = fs::read_to_string(path)?;
        let (mut raw, _) = RawConfig::load_with_defaults(&text)?;
        raw.display.input_lines = input_lines;
        raw.display.accounts_panel_width = Some(accounts_panel_width);
        raw.display.rooms_panel_width_adj = Some(rooms_panel_width_adj);
        let (preserved, _) = comment_invalid_options(&text);
        fs::write(path, raw.rewrite_preserving(&preserved, true)?)?;
        Ok(())
    }

    /// Persist the pinned-room list back to the `[display]` section of the config
    /// file, leaving every other setting and the file's comments intact.
    /// Used to clear leftover local pins after they migrate to `m.favourite`
    /// (ADR 0103); the key stays readable so older TUI builds still parse.
    pub fn save_pinned_rooms(path: &Path, pinned_rooms: &[String]) -> Result<(), ConfigError> {
        let text = fs::read_to_string(path)?;
        let (raw, _) = RawConfig::load_with_defaults(&text)?;
        let (preserved, _) = comment_invalid_options(&text);
        // Reuse the standard rewrite so `[display]` is guaranteed to exist as a
        // regular table before we set the array.
        let rewritten = raw.rewrite_preserving(&preserved, false)?;
        let mut document = rewritten.parse::<DocumentMut>()?;
        // If the user wrote `display = { ... }` (inline table), as_table_mut()
        // returns None; replace it with the regular table from defaults so we can
        // set keys safely (mirrors save_display).
        if document["display"].as_table_mut().is_none() {
            let defaults = raw.to_toml().parse::<DocumentMut>()?;
            document["display"] = defaults["display"].clone();
        }
        let display = document["display"]
            .as_table_mut()
            .expect("display is a regular table after the inline-table fixup");
        let mut array = Array::new();
        for entry in pinned_rooms {
            array.push(entry.as_str());
        }
        display["pinned_rooms"] = Item::Value(Value::Array(array));
        fs::write(path, document.to_string())?;
        Ok(())
    }

    /// Persist the room-list sort + filter mode to the `[display]` section,
    /// leaving every other setting and the file's comments intact. Written
    /// eagerly on each change (ADR 0042). Mirrors [`save_pinned_rooms`].
    pub fn save_room_view(
        path: &Path,
        room_sort: &str,
        room_filter: &str,
    ) -> Result<(), ConfigError> {
        let text = fs::read_to_string(path)?;
        let (raw, _) = RawConfig::load_with_defaults(&text)?;
        let (preserved, _) = comment_invalid_options(&text);
        let rewritten = raw.rewrite_preserving(&preserved, false)?;
        let mut document = rewritten.parse::<DocumentMut>()?;
        if document["display"].as_table_mut().is_none() {
            let defaults = raw.to_toml().parse::<DocumentMut>()?;
            document["display"] = defaults["display"].clone();
        }
        let display = document["display"]
            .as_table_mut()
            .expect("display is a regular table after the inline-table fixup");
        display["room_sort"] = Item::Value(Value::from(room_sort));
        display["room_filter"] = Item::Value(Value::from(room_filter));
        fs::write(path, document.to_string())?;
        Ok(())
    }

    #[cfg(test)]
    pub fn test_default() -> Self {
        let raw = RawConfig::default_values();
        Self {
            shortcuts: raw
                .shortcuts
                .into_shortcuts()
                .expect("default shortcuts parse"),
            colors: raw
                .colors
                .into_color_scheme()
                .expect("default colors parse"),
            display: raw
                .display
                .into_display_options()
                .expect("default display options parse"),
            path: PathBuf::from("/tmp/axon-tui-test-config.toml"),
            created_default: false,
            base_url: None,
            token: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Shortcuts {
    pub next_room: KeyBinding,
    pub previous_room: KeyBinding,
    pub next_account: KeyBinding,
    pub previous_account: KeyBinding,
    pub quit: KeyBinding,
    pub complete: KeyBinding,
    pub submit: KeyBinding,
    pub clear_input: KeyBinding,
    pub backspace: KeyBinding,
    pub cursor_start: KeyBinding,
    pub cursor_end: KeyBinding,
    pub cursor_left: KeyBinding,
    pub cursor_right: KeyBinding,
    pub message_down: KeyBinding,
    pub message_up: KeyBinding,
    pub message_page_up: KeyBinding,
    pub message_page_down: KeyBinding,
    pub edit_previous: KeyBinding,
    pub edit_next: KeyBinding,
    pub media_preview: KeyBinding,
    /// Inserts a hard line break in the compose box (multi-line messages).
    /// Defaults to Alt-Enter, which works across terminals (including tmux);
    /// users on terminals that report it can set this to Shift-Enter.
    pub newline: KeyBinding,
    pub reply: KeyBinding,
    pub thread: KeyBinding,
    pub search_sort: KeyBinding,
    pub search_group: KeyBinding,
    pub search_edit: KeyBinding,
    pub edit_message: KeyBinding,
    pub redact_message: KeyBinding,
    pub react_message: KeyBinding,
    pub unreact_message: KeyBinding,
    pub unread_threads: KeyBinding,
    pub focus_next: KeyBinding,
    pub focus_prev: KeyBinding,
    pub find: KeyBinding,
    pub toggle_accounts_panel: KeyBinding,
    pub toggle_rooms_panel: KeyBinding,
    pub toggle_unread_filter: KeyBinding,
    pub refresh: KeyBinding,
    pub pin_room: KeyBinding,
    pub unpin_room: KeyBinding,
    pub jump_day_back: KeyBinding,
    pub jump_day_forward: KeyBinding,
    // Room-list sort & filter (ADR 0042).
    pub room_filter_cycle: KeyBinding,
    pub room_sort_cycle: KeyBinding,
    pub room_filter_dms: KeyBinding,
    pub room_filter_groups: KeyBinding,
    pub room_filter_favorites: KeyBinding,
    pub room_filter_all: KeyBinding,
    pub room_filter_by_name: KeyBinding,
    pub room_sort_recent: KeyBinding,
    pub room_sort_alpha: KeyBinding,
}

#[derive(Debug, Clone)]
pub struct ColorScheme {
    pub border: Color,
    pub selected_room: Color,
    pub unread_count: Color,
    pub message_sender: Color,
    pub own_message_sender: Color,
    pub input_hint: Color,
    pub status: Color,
    pub accounts_foreground: Color,
    pub rooms_foreground: Color,
    pub messages_foreground: Color,
    pub input_foreground: Color,
    pub accounts_background: Color,
    pub rooms_background: Color,
    pub messages_background: Color,
    pub input_background: Color,
    pub popup_background: Color,
    pub selection_background: Color,
}

#[derive(Debug, Clone)]
pub struct DisplayOptions {
    pub debug: bool,
    pub show_state_events: bool,
    pub message_density: MessageDensity,
    pub time_format: TimeFormat,
    pub input_lines: u16,
    pub max_input_lines: Option<u16>,
    pub preview_warmup_count: usize,
    pub highlight_selected_line: bool,
    pub confirm_logout: bool,
    pub search_wrap: bool,
    /// Whether to surface unsolicited incoming cross-user verification requests
    /// (ADR 0040). When `false`, a verification request from another user is
    /// declined silently; self-verification of your own devices is unaffected.
    pub accept_incoming_verification: bool,
    pub accounts_panel_width: u16,
    pub rooms_panel_width_adj: i16,
    /// Legacy local pins, serialized as `"account_id:room_id"`. Migrated to
    /// `m.favourite` on the first room-list load (ADR 0103); kept so older
    /// TUI builds still parse the file.
    pub pinned_rooms: Vec<String>,
    /// Room-list sort mode token (`recent`/`oldest`/`az`/`za`). See ADR 0042.
    pub room_sort: String,
    /// Room-list filter mode token (`all`/`dms`/`groups`/`unread`/`favorites`).
    /// A name filter persists as `all`. See ADR 0042.
    pub room_filter: String,
}

/// How each message is laid out in the message pane. See ADR 0032 / the
/// dense-vs-normal layout feature.
///
/// - `Dense`: sender, timestamp, and the message start on the same line, with
///   wrapped continuation lines aligned under the body. The sender is shortened
///   to its displayname or `@localpart` (no homeserver).
/// - `Normal`: sender and timestamp sit on their own line and the message
///   begins on the next line, indented to align with the sender. The sender is
///   the displayname if known, otherwise the full mxid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDensity {
    Dense,
    Normal,
}

impl MessageDensity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Dense => "dense",
            Self::Normal => "normal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeFormat {
    H24,
    H12,
}

impl TimeFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::H24 => "24h",
            Self::H12 => "12h",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyBinding {
    pub fn matches(&self, key: KeyEvent) -> bool {
        self.code == key.code && self.modifiers == key.modifiers
    }

    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("Ctrl".to_owned());
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("Alt".to_owned());
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("Shift".to_owned());
        }
        parts.push(match self.code {
            KeyCode::Char(' ') => "Space".to_owned(),
            KeyCode::Char(ch) => ch.to_ascii_uppercase().to_string(),
            KeyCode::Tab => "Tab".to_owned(),
            KeyCode::Enter => "Enter".to_owned(),
            KeyCode::Esc => "Esc".to_owned(),
            KeyCode::Backspace => "Backspace".to_owned(),
            KeyCode::Home => "Home".to_owned(),
            KeyCode::End => "End".to_owned(),
            KeyCode::Up => "Up".to_owned(),
            KeyCode::Down => "Down".to_owned(),
            KeyCode::Left => "Left".to_owned(),
            KeyCode::Right => "Right".to_owned(),
            KeyCode::PageUp => "PageUp".to_owned(),
            KeyCode::PageDown => "PageDown".to_owned(),
            KeyCode::F(n) => format!("F{n}"),
            _ => "?".to_owned(),
        });
        parts.join("-")
    }
}

#[derive(Debug, Clone)]
struct RawConfig {
    shortcuts: RawShortcuts,
    colors: RawColorScheme,
    display: RawDisplayOptions,
    base_url: Option<String>,
    token: Option<String>,
}

impl RawConfig {
    fn load_with_defaults(text: &str) -> Result<(Self, bool), ConfigError> {
        let parsed = toml::from_str::<PartialRawConfig>(text)?;
        let mut raw = Self::default_values();
        let repaired = raw.shortcuts.merge(parsed.shortcuts)
            | raw.colors.merge(parsed.colors)
            | raw.display.merge(parsed.display);
        if let Some(server) = parsed.server {
            raw.token = server.bearer_token;
            raw.base_url = server.base_url;
        }
        Ok((raw, repaired))
    }

    fn default_values() -> Self {
        Self {
            shortcuts: RawShortcuts {
                next_room: "ctrl-n".to_owned(),
                previous_room: "ctrl-p".to_owned(),
                next_account: "alt-n".to_owned(),
                previous_account: "alt-p".to_owned(),
                quit: "ctrl-c".to_owned(),
                complete: "tab".to_owned(),
                submit: "enter".to_owned(),
                clear_input: "esc".to_owned(),
                backspace: "backspace".to_owned(),
                cursor_start: "ctrl-a".to_owned(),
                cursor_end: "ctrl-e".to_owned(),
                cursor_left: "left".to_owned(),
                cursor_right: "right".to_owned(),
                message_down: "ctrl-j".to_owned(),
                message_up: "ctrl-k".to_owned(),
                message_page_up: "pageup".to_owned(),
                message_page_down: "pagedown".to_owned(),
                edit_previous: "up".to_owned(),
                edit_next: "down".to_owned(),
                media_preview: "v".to_owned(),
                newline: "alt-enter".to_owned(),
                reply: "r".to_owned(),
                thread: "t".to_owned(),
                search_sort: "s".to_owned(),
                search_group: "g".to_owned(),
                search_edit: "e".to_owned(),
                edit_message: "e".to_owned(),
                redact_message: "d".to_owned(),
                react_message: "shift-r".to_owned(),
                unreact_message: "shift-u".to_owned(),
                unread_threads: "alt-t".to_owned(),
                focus_next: "f6".to_owned(),
                focus_prev: "ctrl-f6".to_owned(),
                find: "ctrl-f".to_owned(),
                toggle_accounts_panel: "alt-a".to_owned(),
                toggle_rooms_panel: "alt-r".to_owned(),
                toggle_unread_filter: "alt-u".to_owned(),
                refresh: "ctrl-l".to_owned(),
                pin_room: "p".to_owned(),
                unpin_room: "shift-p".to_owned(),
                jump_day_back: "shift-pageup".to_owned(),
                jump_day_forward: "shift-pagedown".to_owned(),
                room_filter_cycle: "alt-f".to_owned(),
                room_sort_cycle: "alt-s".to_owned(),
                room_filter_dms: "alt-d".to_owned(),
                room_filter_groups: "alt-g".to_owned(),
                room_filter_favorites: "alt-v".to_owned(),
                room_filter_all: "alt-0".to_owned(),
                room_filter_by_name: "alt-/".to_owned(),
                room_sort_recent: "alt-1".to_owned(),
                room_sort_alpha: "alt-2".to_owned(),
            },
            colors: RawColorScheme {
                border: "gray".to_owned(),
                selected_room: "cyan".to_owned(),
                unread_count: "yellow".to_owned(),
                message_sender: "green".to_owned(),
                own_message_sender: "light-cyan".to_owned(),
                input_hint: "dark-gray".to_owned(),
                status: "cyan".to_owned(),
                background: "default".to_owned(),
                selection_background: "dark-gray".to_owned(),
                accounts_foreground: None,
                rooms_foreground: None,
                messages_foreground: None,
                input_foreground: None,
                accounts_background: None,
                rooms_background: None,
                messages_background: None,
                input_background: None,
                popup_background: None,
            },
            display: RawDisplayOptions {
                debug: false,
                show_state_events: false,
                message_density: MessageDensity::Normal.as_str().to_owned(),
                time_format: TimeFormat::H24.as_str().to_owned(),
                input_lines: 1,
                max_input_lines: Some(10),
                preview_warmup_count: None,
                highlight_selected_line: false,
                confirm_logout: true,
                search_wrap: true,
                accept_incoming_verification: true,
                accounts_panel_width: None,
                rooms_panel_width_adj: None,
                pinned_rooms: Vec::new(),
                room_sort: "recent".to_owned(),
                room_filter: "all".to_owned(),
            },
            base_url: None,
            token: None,
        }
    }

    fn rewrite_preserving(
        &self,
        original: &str,
        replace_saved_display: bool,
    ) -> Result<String, ConfigError> {
        let mut document = original.parse::<DocumentMut>()?;
        let defaults = self.to_toml().parse::<DocumentMut>()?;

        for section in ["shortcuts", "colors", "display"] {
            merge_missing_table(&mut document, &defaults, section);
        }

        if replace_saved_display {
            // If the user wrote `display = { ... }` (inline table), as_table_mut()
            // returns None. Replace the whole entry with the regular table from
            // defaults so we can update individual keys safely.
            if document["display"].as_table_mut().is_none() {
                document["display"] = defaults["display"].clone();
            }
            let display = document["display"]
                .as_table_mut()
                .expect("display is a regular table");
            replace_integer_preserving_decor(
                display
                    .get_mut("input_lines")
                    .expect("display defaults include input_lines"),
                i64::from(self.display.input_lines),
            );
            replace_integer_preserving_decor(
                display
                    .get_mut("accounts_panel_width")
                    .expect("saved display includes accounts_panel_width"),
                i64::from(
                    self.display
                        .accounts_panel_width
                        .expect("save_display sets accounts_panel_width"),
                ),
            );
            replace_integer_preserving_decor(
                display
                    .get_mut("rooms_panel_width_adj")
                    .expect("saved display includes rooms_panel_width_adj"),
                i64::from(
                    self.display
                        .rooms_panel_width_adj
                        .expect("save_display sets rooms_panel_width_adj"),
                ),
            );
        }

        Ok(document.to_string())
    }

    fn to_toml(&self) -> String {
        let display_extra = {
            let mut s = String::new();
            if let Some(v) = self.display.max_input_lines {
                s.push_str(&format!("max_input_lines = {v}\n"));
            }
            if let Some(v) = self.display.preview_warmup_count {
                s.push_str(&format!("preview_warmup_count = {v}\n"));
            }
            s.push_str(&format!(
                "highlight_selected_line = {}\n",
                self.display.highlight_selected_line
            ));
            if let Some(w) = self.display.accounts_panel_width {
                s.push_str(&format!("accounts_panel_width = {w}\n"));
            }
            if let Some(adj) = self.display.rooms_panel_width_adj {
                s.push_str(&format!("rooms_panel_width_adj = {adj}\n"));
            }
            if !self.display.pinned_rooms.is_empty() {
                s.push_str(&format!(
                    "pinned_rooms = {}\n",
                    toml_string_array(&self.display.pinned_rooms)
                ));
            }
            s.push_str(&format!("room_sort = \"{}\"\n", self.display.room_sort));
            s.push_str(&format!("room_filter = \"{}\"\n", self.display.room_filter));
            s
        };
        let pane_bg_lines = {
            let mut s = String::new();
            if let Some(ref v) = self.colors.accounts_foreground {
                s.push_str(&format!("accounts_foreground = \"{v}\"\n"));
            }
            if let Some(ref v) = self.colors.rooms_foreground {
                s.push_str(&format!("rooms_foreground = \"{v}\"\n"));
            }
            if let Some(ref v) = self.colors.messages_foreground {
                s.push_str(&format!("messages_foreground = \"{v}\"\n"));
            }
            if let Some(ref v) = self.colors.input_foreground {
                s.push_str(&format!("input_foreground = \"{v}\"\n"));
            }
            if let Some(ref v) = self.colors.accounts_background {
                s.push_str(&format!("accounts_background = \"{v}\"\n"));
            }
            if let Some(ref v) = self.colors.rooms_background {
                s.push_str(&format!("rooms_background = \"{v}\"\n"));
            }
            if let Some(ref v) = self.colors.messages_background {
                s.push_str(&format!("messages_background = \"{v}\"\n"));
            }
            if let Some(ref v) = self.colors.input_background {
                s.push_str(&format!("input_background = \"{v}\"\n"));
            }
            if let Some(ref v) = self.colors.popup_background {
                s.push_str(&format!("popup_background = \"{v}\"\n"));
            }
            s
        };
        format!(
            r#"# axon-tui configuration.
#
# Key names are case-insensitive. Supported forms include:
#   ctrl-n, ctrl-p, ctrl-c, ctrl-j, ctrl-k, f6, ctrl-f6, tab, enter, esc,
#   backspace, ctrl-a, ctrl-e, home, end, up, down, left, right,
#   pageup, pagedown, space, r, t, shift-r
#
# Color names are case-insensitive. Supported names:
#   black, red, green, yellow, blue, magenta, cyan, gray,
#   dark-gray, light-red, light-green, light-yellow, light-blue,
#   light-magenta, light-cyan, white, default

[shortcuts]
next_room = "{next_room}"
previous_room = "{previous_room}"
next_account = "{next_account}"
previous_account = "{previous_account}"
quit = "{quit}"
complete = "{complete}"
submit = "{submit}"
clear_input = "{clear_input}"
backspace = "{backspace}"
cursor_start = "{cursor_start}"
cursor_end = "{cursor_end}"
cursor_left = "{cursor_left}"
cursor_right = "{cursor_right}"
message_down = "{message_down}"
message_up = "{message_up}"
message_page_up = "{message_page_up}"
message_page_down = "{message_page_down}"
edit_previous = "{edit_previous}"
edit_next = "{edit_next}"
media_preview = "{media_preview}"
newline = "{newline}"
reply = "{reply}"
thread = "{thread}"
search_sort = "{search_sort}"
search_group = "{search_group}"
search_edit = "{search_edit}"
edit_message = "{edit_message}"
redact_message = "{redact_message}"
react_message = "{react_message}"
unreact_message = "{unreact_message}"
unread_threads = "{unread_threads}"
focus_next = "{focus_next}"
focus_prev = "{focus_prev}"
find = "{find}"
toggle_accounts_panel = "{toggle_accounts_panel}"
toggle_rooms_panel = "{toggle_rooms_panel}"
toggle_unread_filter = "{toggle_unread_filter}"
refresh = "{refresh}"
pin_room = "{pin_room}"
unpin_room = "{unpin_room}"
jump_day_back = "{jump_day_back}"
jump_day_forward = "{jump_day_forward}"
room_filter_cycle = "{room_filter_cycle}"
room_sort_cycle = "{room_sort_cycle}"
room_filter_dms = "{room_filter_dms}"
room_filter_groups = "{room_filter_groups}"
room_filter_favorites = "{room_filter_favorites}"
room_filter_all = "{room_filter_all}"
room_filter_by_name = "{room_filter_by_name}"
room_sort_recent = "{room_sort_recent}"
room_sort_alpha = "{room_sort_alpha}"

# Select one theme by leaving its lines uncommented; comment out all others.
# Named foreground colors: border, selected_room, unread_count, message_sender,
#   own_message_sender, input_hint, status
# Selection background used when display.highlight_selected_line is enabled:
#   selection_background
# Per-pane base foreground (uncolored text) and background:
#   accounts_foreground, rooms_foreground, messages_foreground, input_foreground
#   accounts_background, rooms_background, messages_background, input_background
# Popup background: popup_background
# Global background: background
# All colors accept: black, red, green, yellow, blue, magenta, cyan, gray,
#   dark-gray, light-red, light-green, light-yellow, light-blue, light-magenta,
#   light-cyan, white, default
[colors]
border = "{border}"
selected_room = "{selected_room}"
unread_count = "{unread_count}"
message_sender = "{message_sender}"
own_message_sender = "{own_message_sender}"
input_hint = "{input_hint}"
status = "{status}"
background = "{background}"
selection_background = "{selection_background}"
{pane_bg_lines}
# ── Dracula ───────────────────────────────────────────────────────────────────
# border = "light-magenta"
# selected_room = "light-magenta"
# unread_count = "light-yellow"
# message_sender = "light-cyan"
# own_message_sender = "magenta"
# input_hint = "dark-gray"
# status = "light-magenta"
# background = "black"
# popup_background = "dark-gray"

# ── Nord ──────────────────────────────────────────────────────────────────────
# border = "light-blue"
# selected_room = "light-blue"
# unread_count = "light-yellow"
# message_sender = "light-cyan"
# own_message_sender = "cyan"
# input_hint = "gray"
# status = "light-blue"
# background = "dark-gray"
# accounts_foreground = "white"
# rooms_foreground = "white"
# accounts_background = "black"
# rooms_background = "black"
# popup_background = "dark-gray"

# ── Solarized Dark ────────────────────────────────────────────────────────────
# border = "yellow"
# selected_room = "light-green"
# unread_count = "light-red"
# message_sender = "light-cyan"
# own_message_sender = "cyan"
# input_hint = "dark-gray"
# status = "light-green"
# background = "black"
# popup_background = "dark-gray"

# ── Paper (Light) ─────────────────────────────────────────────────────────────
# border = "gray"
# selected_room = "blue"
# unread_count = "red"
# message_sender = "blue"
# own_message_sender = "dark-gray"
# input_hint = "gray"
# status = "blue"
# background = "white"
# accounts_foreground = "black"
# rooms_foreground = "black"
# messages_foreground = "black"
# input_foreground = "black"
# accounts_background = "gray"
# rooms_background = "gray"
# popup_background = "gray"

[display]
debug = {debug}
show_state_events = {show_state_events}
message_density = "{message_density}"
time_format = "{time_format}"
input_lines = {input_lines}
confirm_logout = {confirm_logout}
search_wrap = {search_wrap}
accept_incoming_verification = {accept_incoming_verification}
{display_extra}"#,
            next_room = self.shortcuts.next_room,
            previous_room = self.shortcuts.previous_room,
            next_account = self.shortcuts.next_account,
            previous_account = self.shortcuts.previous_account,
            quit = self.shortcuts.quit,
            complete = self.shortcuts.complete,
            submit = self.shortcuts.submit,
            clear_input = self.shortcuts.clear_input,
            backspace = self.shortcuts.backspace,
            cursor_start = self.shortcuts.cursor_start,
            cursor_end = self.shortcuts.cursor_end,
            cursor_left = self.shortcuts.cursor_left,
            cursor_right = self.shortcuts.cursor_right,
            message_down = self.shortcuts.message_down,
            message_up = self.shortcuts.message_up,
            message_page_up = self.shortcuts.message_page_up,
            message_page_down = self.shortcuts.message_page_down,
            edit_previous = self.shortcuts.edit_previous,
            edit_next = self.shortcuts.edit_next,
            media_preview = self.shortcuts.media_preview,
            newline = self.shortcuts.newline,
            reply = self.shortcuts.reply,
            thread = self.shortcuts.thread,
            search_sort = self.shortcuts.search_sort,
            search_group = self.shortcuts.search_group,
            search_edit = self.shortcuts.search_edit,
            edit_message = self.shortcuts.edit_message,
            redact_message = self.shortcuts.redact_message,
            react_message = self.shortcuts.react_message,
            unreact_message = self.shortcuts.unreact_message,
            unread_threads = self.shortcuts.unread_threads,
            focus_next = self.shortcuts.focus_next,
            focus_prev = self.shortcuts.focus_prev,
            find = self.shortcuts.find,
            toggle_accounts_panel = self.shortcuts.toggle_accounts_panel,
            toggle_rooms_panel = self.shortcuts.toggle_rooms_panel,
            toggle_unread_filter = self.shortcuts.toggle_unread_filter,
            refresh = self.shortcuts.refresh,
            pin_room = self.shortcuts.pin_room,
            unpin_room = self.shortcuts.unpin_room,
            jump_day_back = self.shortcuts.jump_day_back,
            jump_day_forward = self.shortcuts.jump_day_forward,
            room_filter_cycle = self.shortcuts.room_filter_cycle,
            room_sort_cycle = self.shortcuts.room_sort_cycle,
            room_filter_dms = self.shortcuts.room_filter_dms,
            room_filter_groups = self.shortcuts.room_filter_groups,
            room_filter_favorites = self.shortcuts.room_filter_favorites,
            room_filter_all = self.shortcuts.room_filter_all,
            room_filter_by_name = self.shortcuts.room_filter_by_name,
            room_sort_recent = self.shortcuts.room_sort_recent,
            room_sort_alpha = self.shortcuts.room_sort_alpha,
            border = self.colors.border,
            selected_room = self.colors.selected_room,
            unread_count = self.colors.unread_count,
            message_sender = self.colors.message_sender,
            own_message_sender = self.colors.own_message_sender,
            input_hint = self.colors.input_hint,
            status = self.colors.status,
            background = self.colors.background,
            selection_background = self.colors.selection_background,
            pane_bg_lines = pane_bg_lines,
            debug = self.display.debug,
            show_state_events = self.display.show_state_events,
            message_density = self.display.message_density,
            time_format = self.display.time_format.as_str(),
            input_lines = self.display.input_lines,
            confirm_logout = self.display.confirm_logout,
            search_wrap = self.display.search_wrap,
            accept_incoming_verification = self.display.accept_incoming_verification,
            display_extra = display_extra,
        )
    }
}

/// Render a slice of strings as a TOML inline array literal (`["a", "b"]`),
/// relying on toml_edit to escape each element correctly.
fn toml_string_array(values: &[String]) -> String {
    let mut array = Array::new();
    for value in values {
        array.push(value.as_str());
    }
    Value::Array(array).to_string().trim().to_owned()
}

fn replace_integer_preserving_decor(item: &mut Item, replacement: i64) {
    let decor = item.as_value().map(|value| value.decor().clone());
    let mut replacement = Value::from(replacement);
    if let Some(decor) = decor {
        *replacement.decor_mut() = decor;
    }
    *item = Item::Value(replacement);
}

fn merge_missing_table(document: &mut DocumentMut, defaults: &DocumentMut, section: &str) {
    let Some(default_item) = defaults.get(section) else {
        return;
    };
    if document.get(section).is_none() {
        document[section] = default_item.clone();
        return;
    }

    let Some(table) = document[section].as_table_mut() else {
        return;
    };
    let Some(default_table) = default_item.as_table() else {
        return;
    };
    for (key, item) in default_table {
        if !table.contains_key(key) {
            table.insert(key, item.clone());
        }
    }
}

fn comment_invalid_options(text: &str) -> (String, bool) {
    let mut output = String::with_capacity(text.len());
    let mut section: Option<&str> = None;
    let mut invalid_section = false;
    let mut invalid_option_continuation = false;
    let mut changed = false;

    for line in text.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = content.trim();

        if let Some(table_name) = table_name(trimmed) {
            if is_valid_section(table_name) {
                section = Some(table_name);
                invalid_section = false;
                invalid_option_continuation = false;
                output.push_str(line);
            } else {
                section = None;
                invalid_section = true;
                invalid_option_continuation = false;
                push_commented_invalid_line(&mut output, content, line.ends_with('\n'));
                changed = true;
            }
            continue;
        }

        if trimmed.is_empty() || trimmed.starts_with('#') {
            output.push_str(line);
            continue;
        }

        let key = assignment_key(trimmed);
        let invalid_option = invalid_section
            || key.is_some_and(|key| !is_valid_option(section, key))
            || (key.is_none() && invalid_option_continuation);
        if invalid_option {
            push_commented_invalid_line(&mut output, content, line.ends_with('\n'));
            invalid_option_continuation = !invalid_section;
            changed = true;
        } else {
            invalid_option_continuation = false;
            output.push_str(line);
        }
    }

    (output, changed)
}

fn push_commented_invalid_line(output: &mut String, line: &str, had_newline: bool) {
    output.push_str(INVALID_OPTION_COMMENT);
    output.push('\n');
    output.push_str("# ");
    output.push_str(line);
    if had_newline {
        output.push('\n');
    }
}

fn table_name(line: &str) -> Option<&str> {
    let header = line
        .split_once('#')
        .map_or(line, |(header, _)| header)
        .trim();
    header.strip_prefix('[')?.strip_suffix(']').map(str::trim)
}

fn assignment_key(line: &str) -> Option<&str> {
    line.split_once('=').map(|(key, _)| key.trim())
}

fn is_valid_section(section: &str) -> bool {
    matches!(section, "shortcuts" | "colors" | "display" | "server")
}

fn is_valid_option(section: Option<&str>, key: &str) -> bool {
    match section {
        Some("shortcuts") => matches!(
            key,
            "next_room"
                | "previous_room"
                | "next_account"
                | "previous_account"
                | "quit"
                | "complete"
                | "submit"
                | "clear_input"
                | "backspace"
                | "cursor_start"
                | "cursor_end"
                | "cursor_left"
                | "cursor_right"
                | "edit_previous"
                | "edit_next"
                | "media_preview"
                | "newline"
                | "message_down"
                | "message_up"
                | "message_page_up"
                | "message_page_down"
                | "reply"
                | "thread"
                | "search_sort"
                | "search_group"
                | "search_edit"
                | "edit_message"
                | "redact_message"
                | "react_message"
                | "unreact_message"
                | "unread_threads"
                | "focus_next"
                | "focus_prev"
                | "find"
                | "toggle_accounts_panel"
                | "toggle_rooms_panel"
                | "toggle_unread_filter"
                | "refresh"
                | "pin_room"
                | "unpin_room"
                | "jump_day_back"
                | "jump_day_forward"
                | "room_filter_cycle"
                | "room_sort_cycle"
                | "room_filter_dms"
                | "room_filter_groups"
                | "room_filter_favorites"
                | "room_filter_all"
                | "room_filter_by_name"
                | "room_sort_recent"
                | "room_sort_alpha"
        ),
        Some("colors") => matches!(
            key,
            "border"
                | "selected_room"
                | "unread_count"
                | "message_sender"
                | "own_message_sender"
                | "input_hint"
                | "status"
                | "background"
                | "selection_background"
                | "accounts_foreground"
                | "rooms_foreground"
                | "messages_foreground"
                | "input_foreground"
                | "accounts_background"
                | "rooms_background"
                | "messages_background"
                | "input_background"
                | "popup_background"
        ),
        Some("display") => matches!(
            key,
            "debug"
                | "show_state_events"
                | "message_density"
                | "time_format"
                | "input_lines"
                | "max_input_lines"
                | "preview_warmup_count"
                | "highlight_selected_line"
                | "confirm_logout"
                | "search_wrap"
                | "accept_incoming_verification"
                | "accounts_panel_width"
                | "rooms_panel_width_adj"
                | "pinned_rooms"
                | "room_sort"
                | "room_filter"
        ),
        Some("server") => matches!(key, "base_url" | "bearer_token"),
        _ => false,
    }
}

#[derive(Debug, Default, Deserialize)]
struct PartialRawConfig {
    shortcuts: Option<PartialRawShortcuts>,
    colors: Option<PartialRawColorScheme>,
    display: Option<PartialDisplayOptions>,
    server: Option<PartialRawServer>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialRawServer {
    base_url: Option<String>,
    bearer_token: Option<String>,
}

#[derive(Debug, Clone)]
struct RawShortcuts {
    next_room: String,
    previous_room: String,
    next_account: String,
    previous_account: String,
    quit: String,
    complete: String,
    submit: String,
    clear_input: String,
    backspace: String,
    cursor_start: String,
    cursor_end: String,
    cursor_left: String,
    cursor_right: String,
    message_down: String,
    message_up: String,
    message_page_up: String,
    message_page_down: String,
    edit_previous: String,
    edit_next: String,
    media_preview: String,
    newline: String,
    reply: String,
    thread: String,
    search_sort: String,
    search_group: String,
    search_edit: String,
    edit_message: String,
    redact_message: String,
    react_message: String,
    unreact_message: String,
    unread_threads: String,
    focus_next: String,
    focus_prev: String,
    find: String,
    toggle_accounts_panel: String,
    toggle_rooms_panel: String,
    toggle_unread_filter: String,
    refresh: String,
    pin_room: String,
    unpin_room: String,
    jump_day_back: String,
    jump_day_forward: String,
    room_filter_cycle: String,
    room_sort_cycle: String,
    room_filter_dms: String,
    room_filter_groups: String,
    room_filter_favorites: String,
    room_filter_all: String,
    room_filter_by_name: String,
    room_sort_recent: String,
    room_sort_alpha: String,
}

impl RawShortcuts {
    fn merge(&mut self, partial: Option<PartialRawShortcuts>) -> bool {
        let Some(partial) = partial else {
            return true;
        };
        let mut missing = false;
        assign_or_flag(&mut self.next_room, partial.next_room, &mut missing);
        assign_or_flag(&mut self.previous_room, partial.previous_room, &mut missing);
        assign_or_flag(&mut self.next_account, partial.next_account, &mut missing);
        assign_or_flag(
            &mut self.previous_account,
            partial.previous_account,
            &mut missing,
        );
        assign_or_flag(&mut self.quit, partial.quit, &mut missing);
        assign_or_flag(&mut self.complete, partial.complete, &mut missing);
        assign_or_flag(&mut self.submit, partial.submit, &mut missing);
        assign_or_flag(&mut self.clear_input, partial.clear_input, &mut missing);
        assign_or_flag(&mut self.backspace, partial.backspace, &mut missing);
        assign_or_flag(&mut self.cursor_start, partial.cursor_start, &mut missing);
        assign_or_flag(&mut self.cursor_end, partial.cursor_end, &mut missing);
        assign_or_flag(&mut self.cursor_left, partial.cursor_left, &mut missing);
        assign_or_flag(&mut self.cursor_right, partial.cursor_right, &mut missing);
        assign_or_flag(&mut self.edit_previous, partial.edit_previous, &mut missing);
        assign_or_flag(&mut self.edit_next, partial.edit_next, &mut missing);
        assign_or_flag(&mut self.media_preview, partial.media_preview, &mut missing);
        assign_or_flag(&mut self.newline, partial.newline, &mut missing);
        assign_or_flag(&mut self.message_down, partial.message_down, &mut missing);
        assign_or_flag(&mut self.message_up, partial.message_up, &mut missing);
        assign_or_flag(
            &mut self.message_page_up,
            partial.message_page_up,
            &mut missing,
        );
        assign_or_flag(
            &mut self.message_page_down,
            partial.message_page_down,
            &mut missing,
        );
        assign_or_flag(&mut self.reply, partial.reply, &mut missing);
        assign_or_flag(&mut self.thread, partial.thread, &mut missing);
        assign_or_flag(&mut self.search_sort, partial.search_sort, &mut missing);
        assign_or_flag(&mut self.search_group, partial.search_group, &mut missing);
        assign_or_flag(&mut self.search_edit, partial.search_edit, &mut missing);
        assign_or_flag(&mut self.edit_message, partial.edit_message, &mut missing);
        assign_or_flag(
            &mut self.redact_message,
            partial.redact_message,
            &mut missing,
        );
        assign_or_flag(&mut self.react_message, partial.react_message, &mut missing);
        assign_or_flag(
            &mut self.unreact_message,
            partial.unreact_message,
            &mut missing,
        );
        assign_or_flag(
            &mut self.unread_threads,
            partial.unread_threads,
            &mut missing,
        );
        assign_or_flag(&mut self.focus_next, partial.focus_next, &mut missing);
        assign_or_flag(&mut self.focus_prev, partial.focus_prev, &mut missing);
        assign_or_flag(&mut self.find, partial.find, &mut missing);
        assign_or_flag(
            &mut self.toggle_accounts_panel,
            partial.toggle_accounts_panel,
            &mut missing,
        );
        assign_or_flag(
            &mut self.toggle_rooms_panel,
            partial.toggle_rooms_panel,
            &mut missing,
        );
        assign_or_flag(
            &mut self.toggle_unread_filter,
            partial.toggle_unread_filter,
            &mut missing,
        );
        assign_or_flag(&mut self.refresh, partial.refresh, &mut missing);
        assign_or_flag(&mut self.pin_room, partial.pin_room, &mut missing);
        assign_or_flag(&mut self.unpin_room, partial.unpin_room, &mut missing);
        assign_or_flag(&mut self.jump_day_back, partial.jump_day_back, &mut missing);
        assign_or_flag(
            &mut self.jump_day_forward,
            partial.jump_day_forward,
            &mut missing,
        );
        assign_or_flag(
            &mut self.room_filter_cycle,
            partial.room_filter_cycle,
            &mut missing,
        );
        assign_or_flag(
            &mut self.room_sort_cycle,
            partial.room_sort_cycle,
            &mut missing,
        );
        assign_or_flag(
            &mut self.room_filter_dms,
            partial.room_filter_dms,
            &mut missing,
        );
        assign_or_flag(
            &mut self.room_filter_groups,
            partial.room_filter_groups,
            &mut missing,
        );
        assign_or_flag(
            &mut self.room_filter_favorites,
            partial.room_filter_favorites,
            &mut missing,
        );
        assign_or_flag(
            &mut self.room_filter_all,
            partial.room_filter_all,
            &mut missing,
        );
        assign_or_flag(
            &mut self.room_filter_by_name,
            partial.room_filter_by_name,
            &mut missing,
        );
        assign_or_flag(
            &mut self.room_sort_recent,
            partial.room_sort_recent,
            &mut missing,
        );
        assign_or_flag(
            &mut self.room_sort_alpha,
            partial.room_sort_alpha,
            &mut missing,
        );
        missing
    }

    fn into_shortcuts(self) -> Result<Shortcuts, ConfigError> {
        Ok(Shortcuts {
            next_room: parse_key_binding("shortcuts.next_room", &self.next_room)?,
            previous_room: parse_key_binding("shortcuts.previous_room", &self.previous_room)?,
            next_account: parse_key_binding("shortcuts.next_account", &self.next_account)?,
            previous_account: parse_key_binding(
                "shortcuts.previous_account",
                &self.previous_account,
            )?,
            quit: parse_key_binding("shortcuts.quit", &self.quit)?,
            complete: parse_key_binding("shortcuts.complete", &self.complete)?,
            submit: parse_key_binding("shortcuts.submit", &self.submit)?,
            clear_input: parse_key_binding("shortcuts.clear_input", &self.clear_input)?,
            backspace: parse_key_binding("shortcuts.backspace", &self.backspace)?,
            cursor_start: parse_key_binding("shortcuts.cursor_start", &self.cursor_start)?,
            cursor_end: parse_key_binding("shortcuts.cursor_end", &self.cursor_end)?,
            cursor_left: parse_key_binding("shortcuts.cursor_left", &self.cursor_left)?,
            cursor_right: parse_key_binding("shortcuts.cursor_right", &self.cursor_right)?,
            message_down: parse_key_binding("shortcuts.message_down", &self.message_down)?,
            message_up: parse_key_binding("shortcuts.message_up", &self.message_up)?,
            message_page_up: parse_key_binding("shortcuts.message_page_up", &self.message_page_up)?,
            message_page_down: parse_key_binding(
                "shortcuts.message_page_down",
                &self.message_page_down,
            )?,
            edit_previous: parse_key_binding("shortcuts.edit_previous", &self.edit_previous)?,
            edit_next: parse_key_binding("shortcuts.edit_next", &self.edit_next)?,
            media_preview: parse_key_binding("shortcuts.media_preview", &self.media_preview)?,
            newline: parse_key_binding("shortcuts.newline", &self.newline)?,
            reply: parse_key_binding("shortcuts.reply", &self.reply)?,
            thread: parse_key_binding("shortcuts.thread", &self.thread)?,
            search_sort: parse_key_binding("shortcuts.search_sort", &self.search_sort)?,
            search_group: parse_key_binding("shortcuts.search_group", &self.search_group)?,
            search_edit: parse_key_binding("shortcuts.search_edit", &self.search_edit)?,
            edit_message: parse_key_binding("shortcuts.edit_message", &self.edit_message)?,
            redact_message: parse_key_binding("shortcuts.redact_message", &self.redact_message)?,
            react_message: parse_key_binding("shortcuts.react_message", &self.react_message)?,
            unreact_message: parse_key_binding("shortcuts.unreact_message", &self.unreact_message)?,
            unread_threads: parse_key_binding("shortcuts.unread_threads", &self.unread_threads)?,
            focus_next: parse_key_binding("shortcuts.focus_next", &self.focus_next)?,
            focus_prev: parse_key_binding("shortcuts.focus_prev", &self.focus_prev)?,
            find: parse_key_binding("shortcuts.find", &self.find)?,
            toggle_accounts_panel: parse_key_binding(
                "shortcuts.toggle_accounts_panel",
                &self.toggle_accounts_panel,
            )?,
            toggle_rooms_panel: parse_key_binding(
                "shortcuts.toggle_rooms_panel",
                &self.toggle_rooms_panel,
            )?,
            toggle_unread_filter: parse_key_binding(
                "shortcuts.toggle_unread_filter",
                &self.toggle_unread_filter,
            )?,
            refresh: parse_key_binding("shortcuts.refresh", &self.refresh)?,
            pin_room: parse_key_binding("shortcuts.pin_room", &self.pin_room)?,
            unpin_room: parse_key_binding("shortcuts.unpin_room", &self.unpin_room)?,
            jump_day_back: parse_key_binding("shortcuts.jump_day_back", &self.jump_day_back)?,
            jump_day_forward: parse_key_binding(
                "shortcuts.jump_day_forward",
                &self.jump_day_forward,
            )?,
            room_filter_cycle: parse_key_binding(
                "shortcuts.room_filter_cycle",
                &self.room_filter_cycle,
            )?,
            room_sort_cycle: parse_key_binding("shortcuts.room_sort_cycle", &self.room_sort_cycle)?,
            room_filter_dms: parse_key_binding("shortcuts.room_filter_dms", &self.room_filter_dms)?,
            room_filter_groups: parse_key_binding(
                "shortcuts.room_filter_groups",
                &self.room_filter_groups,
            )?,
            room_filter_favorites: parse_key_binding(
                "shortcuts.room_filter_favorites",
                &self.room_filter_favorites,
            )?,
            room_filter_all: parse_key_binding("shortcuts.room_filter_all", &self.room_filter_all)?,
            room_filter_by_name: parse_key_binding(
                "shortcuts.room_filter_by_name",
                &self.room_filter_by_name,
            )?,
            room_sort_recent: parse_key_binding(
                "shortcuts.room_sort_recent",
                &self.room_sort_recent,
            )?,
            room_sort_alpha: parse_key_binding("shortcuts.room_sort_alpha", &self.room_sort_alpha)?,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
struct PartialRawShortcuts {
    next_room: Option<String>,
    previous_room: Option<String>,
    next_account: Option<String>,
    previous_account: Option<String>,
    quit: Option<String>,
    complete: Option<String>,
    submit: Option<String>,
    clear_input: Option<String>,
    backspace: Option<String>,
    cursor_start: Option<String>,
    cursor_end: Option<String>,
    cursor_left: Option<String>,
    cursor_right: Option<String>,
    message_down: Option<String>,
    message_up: Option<String>,
    message_page_up: Option<String>,
    message_page_down: Option<String>,
    #[serde(alias = "history_previous")]
    edit_previous: Option<String>,
    #[serde(alias = "history_next")]
    edit_next: Option<String>,
    media_preview: Option<String>,
    newline: Option<String>,
    reply: Option<String>,
    thread: Option<String>,
    search_sort: Option<String>,
    search_group: Option<String>,
    search_edit: Option<String>,
    edit_message: Option<String>,
    redact_message: Option<String>,
    react_message: Option<String>,
    unreact_message: Option<String>,
    unread_threads: Option<String>,
    focus_next: Option<String>,
    focus_prev: Option<String>,
    find: Option<String>,
    toggle_accounts_panel: Option<String>,
    toggle_rooms_panel: Option<String>,
    toggle_unread_filter: Option<String>,
    refresh: Option<String>,
    pin_room: Option<String>,
    unpin_room: Option<String>,
    jump_day_back: Option<String>,
    jump_day_forward: Option<String>,
    room_filter_cycle: Option<String>,
    room_sort_cycle: Option<String>,
    room_filter_dms: Option<String>,
    room_filter_groups: Option<String>,
    room_filter_favorites: Option<String>,
    room_filter_all: Option<String>,
    room_filter_by_name: Option<String>,
    room_sort_recent: Option<String>,
    room_sort_alpha: Option<String>,
}

#[derive(Debug, Clone)]
struct RawColorScheme {
    border: String,
    selected_room: String,
    unread_count: String,
    message_sender: String,
    own_message_sender: String,
    input_hint: String,
    status: String,
    background: String,
    selection_background: String,
    accounts_foreground: Option<String>,
    rooms_foreground: Option<String>,
    messages_foreground: Option<String>,
    input_foreground: Option<String>,
    accounts_background: Option<String>,
    rooms_background: Option<String>,
    messages_background: Option<String>,
    input_background: Option<String>,
    popup_background: Option<String>,
}

impl RawColorScheme {
    fn merge(&mut self, partial: Option<PartialRawColorScheme>) -> bool {
        let Some(partial) = partial else {
            return true;
        };
        let mut missing = false;
        assign_or_flag(&mut self.border, partial.border, &mut missing);
        assign_or_flag(&mut self.selected_room, partial.selected_room, &mut missing);
        assign_or_flag(&mut self.unread_count, partial.unread_count, &mut missing);
        assign_or_flag(
            &mut self.message_sender,
            partial.message_sender,
            &mut missing,
        );
        assign_or_flag(
            &mut self.own_message_sender,
            partial.own_message_sender,
            &mut missing,
        );
        assign_or_flag(&mut self.input_hint, partial.input_hint, &mut missing);
        assign_or_flag(&mut self.status, partial.status, &mut missing);
        assign_or_flag(&mut self.background, partial.background, &mut missing);
        assign_or_flag(
            &mut self.selection_background,
            partial.selection_background,
            &mut missing,
        );
        if let Some(v) = partial.accounts_foreground {
            self.accounts_foreground = Some(v);
        }
        if let Some(v) = partial.rooms_foreground {
            self.rooms_foreground = Some(v);
        }
        if let Some(v) = partial.messages_foreground {
            self.messages_foreground = Some(v);
        }
        if let Some(v) = partial.input_foreground {
            self.input_foreground = Some(v);
        }
        if let Some(v) = partial.accounts_background {
            self.accounts_background = Some(v);
        }
        if let Some(v) = partial.rooms_background {
            self.rooms_background = Some(v);
        }
        if let Some(v) = partial.messages_background {
            self.messages_background = Some(v);
        }
        if let Some(v) = partial.input_background {
            self.input_background = Some(v);
        }
        if let Some(v) = partial.popup_background {
            self.popup_background = Some(v);
        }
        missing
    }

    fn into_color_scheme(self) -> Result<ColorScheme, ConfigError> {
        let background = parse_color("colors.background", &self.background)?;
        let accounts_foreground = self
            .accounts_foreground
            .as_deref()
            .map(|v| parse_color("colors.accounts_foreground", v))
            .transpose()?
            .unwrap_or(Color::Reset);
        let rooms_foreground = self
            .rooms_foreground
            .as_deref()
            .map(|v| parse_color("colors.rooms_foreground", v))
            .transpose()?
            .unwrap_or(Color::Reset);
        let messages_foreground = self
            .messages_foreground
            .as_deref()
            .map(|v| parse_color("colors.messages_foreground", v))
            .transpose()?
            .unwrap_or(Color::Reset);
        let input_foreground = self
            .input_foreground
            .as_deref()
            .map(|v| parse_color("colors.input_foreground", v))
            .transpose()?
            .unwrap_or(Color::Reset);
        let accounts_background = self
            .accounts_background
            .as_deref()
            .map(|v| parse_color("colors.accounts_background", v))
            .transpose()?
            .unwrap_or(background);
        let rooms_background = self
            .rooms_background
            .as_deref()
            .map(|v| parse_color("colors.rooms_background", v))
            .transpose()?
            .unwrap_or(background);
        let messages_background = self
            .messages_background
            .as_deref()
            .map(|v| parse_color("colors.messages_background", v))
            .transpose()?
            .unwrap_or(background);
        let input_background = self
            .input_background
            .as_deref()
            .map(|v| parse_color("colors.input_background", v))
            .transpose()?
            .unwrap_or(background);
        let popup_background = self
            .popup_background
            .as_deref()
            .map(|v| parse_color("colors.popup_background", v))
            .transpose()?
            .unwrap_or(background);
        Ok(ColorScheme {
            border: parse_color("colors.border", &self.border)?,
            selected_room: parse_color("colors.selected_room", &self.selected_room)?,
            unread_count: parse_color("colors.unread_count", &self.unread_count)?,
            message_sender: parse_color("colors.message_sender", &self.message_sender)?,
            own_message_sender: parse_color("colors.own_message_sender", &self.own_message_sender)?,
            input_hint: parse_color("colors.input_hint", &self.input_hint)?,
            status: parse_color("colors.status", &self.status)?,
            accounts_foreground,
            rooms_foreground,
            messages_foreground,
            input_foreground,
            accounts_background,
            rooms_background,
            messages_background,
            input_background,
            popup_background,
            selection_background: parse_color(
                "colors.selection_background",
                &self.selection_background,
            )?,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
struct PartialRawColorScheme {
    border: Option<String>,
    selected_room: Option<String>,
    unread_count: Option<String>,
    message_sender: Option<String>,
    own_message_sender: Option<String>,
    input_hint: Option<String>,
    status: Option<String>,
    background: Option<String>,
    selection_background: Option<String>,
    accounts_foreground: Option<String>,
    rooms_foreground: Option<String>,
    messages_foreground: Option<String>,
    input_foreground: Option<String>,
    accounts_background: Option<String>,
    rooms_background: Option<String>,
    messages_background: Option<String>,
    input_background: Option<String>,
    popup_background: Option<String>,
}

#[derive(Debug, Clone)]
struct RawDisplayOptions {
    debug: bool,
    show_state_events: bool,
    message_density: String,
    time_format: String,
    input_lines: u16,
    max_input_lines: Option<u16>,
    preview_warmup_count: Option<u8>,
    highlight_selected_line: bool,
    confirm_logout: bool,
    search_wrap: bool,
    accept_incoming_verification: bool,
    accounts_panel_width: Option<u16>,
    rooms_panel_width_adj: Option<i16>,
    pinned_rooms: Vec<String>,
    room_sort: String,
    room_filter: String,
}

impl RawDisplayOptions {
    fn merge(&mut self, partial: Option<PartialDisplayOptions>) -> bool {
        let Some(partial) = partial else {
            return true;
        };
        let mut missing = false;
        if let Some(debug) = partial.debug {
            self.debug = debug;
        } else {
            missing = true;
        }
        if let Some(show_state_events) = partial.show_state_events {
            self.show_state_events = show_state_events;
        } else {
            missing = true;
        }
        if let Some(message_density) = partial.message_density {
            self.message_density = message_density;
        } else {
            missing = true;
        }
        if let Some(time_format) = partial.time_format {
            self.time_format = time_format;
        } else {
            missing = true;
        }
        if let Some(input_lines) = partial.input_lines {
            self.input_lines = input_lines.clamp(1, 10);
        } else {
            missing = true;
        }
        if let Some(v) = partial.max_input_lines {
            self.max_input_lines = Some(v.clamp(1, 100));
        } else {
            missing = true;
        }
        if let Some(v) = partial.preview_warmup_count {
            self.preview_warmup_count = Some(v);
        } else {
            missing = true;
        }
        if let Some(highlight_selected_line) = partial.highlight_selected_line {
            self.highlight_selected_line = highlight_selected_line;
        } else {
            missing = true;
        }
        if let Some(confirm_logout) = partial.confirm_logout {
            self.confirm_logout = confirm_logout;
        } else {
            missing = true;
        }
        if let Some(search_wrap) = partial.search_wrap {
            self.search_wrap = search_wrap;
        } else {
            missing = true;
        }
        if let Some(accept_incoming_verification) = partial.accept_incoming_verification {
            self.accept_incoming_verification = accept_incoming_verification;
        } else {
            missing = true;
        }
        if let Some(v) = partial.accounts_panel_width {
            self.accounts_panel_width = Some(v);
        }
        if let Some(v) = partial.rooms_panel_width_adj {
            self.rooms_panel_width_adj = Some(v);
        }
        if let Some(v) = partial.pinned_rooms {
            self.pinned_rooms = v;
        }
        if let Some(v) = partial.room_sort {
            self.room_sort = v;
        }
        if let Some(v) = partial.room_filter {
            self.room_filter = v;
        }
        if let Some(max_input_lines) = self.max_input_lines {
            let clamped_input_lines = self.input_lines.min(max_input_lines);
            if clamped_input_lines != self.input_lines {
                self.input_lines = clamped_input_lines;
                missing = true;
            }
        }
        missing
    }

    fn into_display_options(self) -> Result<DisplayOptions, ConfigError> {
        let max_input_lines = self.max_input_lines.map(|v| v.clamp(1, 100));
        let input_lines = self
            .input_lines
            .clamp(1, 10)
            .min(max_input_lines.unwrap_or(u16::MAX));
        Ok(DisplayOptions {
            debug: self.debug,
            show_state_events: self.show_state_events,
            message_density: parse_message_density(
                "display.message_density",
                &self.message_density,
            )?,
            time_format: parse_time_format("display.time_format", &self.time_format)?,
            input_lines,
            max_input_lines,
            preview_warmup_count: self.preview_warmup_count.unwrap_or(5) as usize,
            highlight_selected_line: self.highlight_selected_line,
            confirm_logout: self.confirm_logout,
            search_wrap: self.search_wrap,
            accept_incoming_verification: self.accept_incoming_verification,
            accounts_panel_width: self
                .accounts_panel_width
                .unwrap_or(DEFAULT_ACCOUNTS_PANEL_WIDTH),
            rooms_panel_width_adj: self.rooms_panel_width_adj.unwrap_or(0),
            pinned_rooms: self.pinned_rooms,
            room_sort: self.room_sort,
            room_filter: self.room_filter,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
struct PartialDisplayOptions {
    debug: Option<bool>,
    show_state_events: Option<bool>,
    message_density: Option<String>,
    time_format: Option<String>,
    input_lines: Option<u16>,
    max_input_lines: Option<u16>,
    preview_warmup_count: Option<u8>,
    highlight_selected_line: Option<bool>,
    confirm_logout: Option<bool>,
    search_wrap: Option<bool>,
    accept_incoming_verification: Option<bool>,
    accounts_panel_width: Option<u16>,
    rooms_panel_width_adj: Option<i16>,
    pinned_rooms: Option<Vec<String>>,
    room_sort: Option<String>,
    room_filter: Option<String>,
}

fn assign_or_flag(target: &mut String, value: Option<String>, missing: &mut bool) {
    match value {
        Some(v) => *target = v,
        None => *missing = true,
    }
}

fn ensure_default_config(path: &Path) -> Result<bool, ConfigError> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, DEFAULT_CONFIG)?;
    Ok(true)
}

fn config_path() -> Result<PathBuf, ConfigError> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir).join("axon-tui").join("config.toml"));
    }
    let home = env::var_os("HOME").ok_or(ConfigError::MissingHome)?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("axon-tui")
        .join("config.toml"))
}

fn parse_key_binding(field: &'static str, value: &str) -> Result<KeyBinding, ConfigError> {
    let mut modifiers = KeyModifiers::empty();
    let mut key = None;
    for part in value.split('-') {
        let part = part.trim().to_ascii_lowercase();
        match part.as_str() {
            "" => {}
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "alt" | "meta" => modifiers |= KeyModifiers::ALT,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            "tab" => key = Some(KeyCode::Tab),
            "enter" | "return" => key = Some(KeyCode::Enter),
            "esc" | "escape" => key = Some(KeyCode::Esc),
            "backspace" => key = Some(KeyCode::Backspace),
            "home" => key = Some(KeyCode::Home),
            "end" => key = Some(KeyCode::End),
            "up" => key = Some(KeyCode::Up),
            "down" => key = Some(KeyCode::Down),
            "left" => key = Some(KeyCode::Left),
            "right" => key = Some(KeyCode::Right),
            "pageup" | "page_up" | "pgup" => key = Some(KeyCode::PageUp),
            "pagedown" | "page_down" | "pgdn" => key = Some(KeyCode::PageDown),
            "space" => key = Some(KeyCode::Char(' ')),
            key_name if key_name.starts_with('f') && key_name[1..].parse::<u8>().is_ok() => {
                key = Some(KeyCode::F(key_name[1..].parse().unwrap()));
            }
            key_name if key_name.chars().count() == 1 => {
                key = key_name.chars().next().map(KeyCode::Char);
            }
            _ => {
                return Err(ConfigError::InvalidKey {
                    field,
                    value: value.to_owned(),
                });
            }
        }
    }
    let mut code = key.ok_or_else(|| ConfigError::InvalidKey {
        field,
        value: value.to_owned(),
    })?;
    if modifiers.contains(KeyModifiers::SHIFT) {
        if let KeyCode::Char(ch) = code {
            code = KeyCode::Char(ch.to_ascii_uppercase());
        }
    }
    Ok(KeyBinding { code, modifiers })
}

fn parse_color(field: &'static str, value: &str) -> Result<Color, ConfigError> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "black" => Ok(Color::Black),
        "red" => Ok(Color::Red),
        "green" => Ok(Color::Green),
        "yellow" => Ok(Color::Yellow),
        "blue" => Ok(Color::Blue),
        "magenta" => Ok(Color::Magenta),
        "cyan" => Ok(Color::Cyan),
        "gray" | "grey" => Ok(Color::Gray),
        "dark-gray" | "dark-grey" => Ok(Color::DarkGray),
        "light-red" => Ok(Color::LightRed),
        "light-green" => Ok(Color::LightGreen),
        "light-yellow" => Ok(Color::LightYellow),
        "light-blue" => Ok(Color::LightBlue),
        "light-magenta" => Ok(Color::LightMagenta),
        "light-cyan" => Ok(Color::LightCyan),
        "white" => Ok(Color::White),
        "default" | "reset" => Ok(Color::Reset),
        _ => Err(ConfigError::InvalidColor {
            field,
            value: value.to_owned(),
        }),
    }
}

fn parse_message_density(field: &'static str, value: &str) -> Result<MessageDensity, ConfigError> {
    match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "normal" | "two_line" | "expanded" => Ok(MessageDensity::Normal),
        "dense" | "compact" | "one_line" => Ok(MessageDensity::Dense),
        _ => Err(ConfigError::InvalidDisplayOption {
            field,
            value: value.to_owned(),
        }),
    }
}

fn parse_time_format(field: &'static str, value: &str) -> Result<TimeFormat, ConfigError> {
    match value.trim().to_ascii_lowercase().replace('-', "").as_str() {
        "24h" | "24" => Ok(TimeFormat::H24),
        "12h" | "12" => Ok(TimeFormat::H12),
        _ => Err(ConfigError::InvalidDisplayOption {
            field,
            value: value.to_owned(),
        }),
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not determine home directory for axon-tui config")]
    MissingHome,
    #[error("config file I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("config TOML is invalid: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("config TOML could not be rewritten: {0}")]
    TomlEdit(#[from] toml_edit::TomlError),
    #[error("invalid key binding for {field}: {value}")]
    InvalidKey { field: &'static str, value: String },
    #[error("invalid color for {field}: {value}")]
    InvalidColor { field: &'static str, value: String },
    #[error("invalid display option for {field}: {value}")]
    InvalidDisplayOption { field: &'static str, value: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_default_config_when_missing() {
        let path =
            env::temp_dir().join(format!("axon-tui-test-{}-config.toml", std::process::id()));
        let _ = fs::remove_file(&path);

        let config = TuiConfig::load_or_create_at(path.clone()).expect("load");

        assert!(config.created_default);
        assert!(path.exists());
        assert!(fs::read_to_string(&path).unwrap().contains("[shortcuts]"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn parses_default_config() {
        let (raw, _) =
            RawConfig::load_with_defaults(DEFAULT_CONFIG).expect("default config parses");
        let shortcuts = raw.shortcuts.into_shortcuts().expect("shortcuts");

        assert!(shortcuts
            .next_room
            .matches(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)));
        assert!(shortcuts.edit_previous.matches(KeyEvent::from(KeyCode::Up)));
        assert!(shortcuts.edit_next.matches(KeyEvent::from(KeyCode::Down)));
        assert!(shortcuts
            .media_preview
            .matches(KeyEvent::from(KeyCode::Char('v'))));
    }

    #[test]
    fn parses_custom_edit_navigation_and_media_preview_shortcuts() {
        let raw = RawConfig::load_with_defaults(
            r#"[shortcuts]
edit_previous = "ctrl-b"
edit_next = "ctrl-f"
media_preview = "i"
"#,
        )
        .expect("custom config parses");
        let shortcuts = raw.0.shortcuts.into_shortcuts().expect("shortcuts");

        assert!(shortcuts
            .edit_previous
            .matches(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)));
        assert!(shortcuts
            .edit_next
            .matches(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)));
        assert!(shortcuts
            .media_preview
            .matches(KeyEvent::from(KeyCode::Char('i'))));
    }

    #[test]
    fn newline_defaults_to_alt_enter_and_accepts_shift_enter() {
        // Default: Alt+Enter (works across terminals incl. tmux); plain Enter
        // must not match so it still submits.
        let default = TuiConfig::test_default();
        assert!(default
            .shortcuts
            .newline
            .matches(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
        assert!(!default
            .shortcuts
            .newline
            .matches(KeyEvent::from(KeyCode::Enter)));

        // Users on capable terminals can rebind it to Shift+Enter.
        let raw = RawConfig::load_with_defaults(
            r#"[shortcuts]
newline = "shift-enter"
"#,
        )
        .expect("custom config parses");
        let shortcuts = raw.0.shortcuts.into_shortcuts().expect("shortcuts");
        assert!(shortcuts
            .newline
            .matches(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)));
    }

    #[test]
    fn repairs_config_missing_newer_fields() {
        let path = env::temp_dir().join(format!(
            "axon-tui-test-{}-repair-config.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        fs::write(
            &path,
            r#"[shortcuts]
next_room = "ctrl-n"
previous_room = "ctrl-p"
quit = "ctrl-c"
complete = "tab"
submit = "enter"
clear_input = "esc"
backspace = "backspace"

[colors]
border = "gray"
selected_room = "cyan"
unread_count = "yellow"
message_sender = "green"
input_hint = "dark-gray"
status = "cyan"

[server]
base_url = "https://axon.example.com"
bearer_token = "secret-token"
"#,
        )
        .expect("write old config");

        let config = TuiConfig::load_or_create_at(path.clone()).expect("load repaired config");

        assert!(!config.created_default);
        assert!(config
            .shortcuts
            .cursor_start
            .matches(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)));
        let repaired = fs::read_to_string(&path).expect("read repaired config");
        assert!(repaired.contains("cursor_start = \"ctrl-a\""));
        assert!(repaired.contains("message_down = \"ctrl-j\""));
        assert!(repaired.contains("message_page_up = \"pageup\""));
        assert!(repaired.contains("edit_previous = \"up\""));
        assert!(repaired.contains("edit_next = \"down\""));
        assert!(repaired.contains("media_preview = \"v\""));
        assert!(repaired.contains("thread = \"t\""));
        assert!(repaired.contains("unreact_message = \"shift-u\""));
        assert!(repaired.contains("focus_next = \"f6\""));
        assert!(repaired.contains("focus_prev = \"ctrl-f6\""));
        assert!(repaired.contains("own_message_sender = \"light-cyan\""));
        assert!(repaired.contains("selection_background = \"dark-gray\""));
        assert!(repaired.contains("[display]"));
        assert!(repaired.contains("debug = false"));
        assert!(repaired.contains("show_state_events = false"));
        assert!(repaired.contains("message_density = \"normal\""));
        assert!(repaired.contains("highlight_selected_line = false"));
        assert!(repaired.contains("confirm_logout = true"));
        assert!(repaired.contains("base_url = \"https://axon.example.com\""));
        assert!(repaired.contains("bearer_token = \"secret-token\""));
        assert_eq!(config.base_url.as_deref(), Some("https://axon.example.com"));
        assert_eq!(config.token.as_deref(), Some("secret-token"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn accepts_legacy_history_shortcut_keys() {
        let path = env::temp_dir().join(format!(
            "axon-tui-test-{}-legacy-history-config.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        fs::write(
            &path,
            r#"[shortcuts]
history_previous = "ctrl-k"
history_next = "ctrl-j"
"#,
        )
        .expect("write legacy config");

        let config = TuiConfig::load_or_create_at(path.clone()).expect("load legacy config");

        assert!(config
            .shortcuts
            .edit_previous
            .matches(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)));
        assert!(config
            .shortcuts
            .edit_next
            .matches(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)));
        let repaired = fs::read_to_string(&path).expect("read repaired config");
        assert!(repaired.contains("edit_previous = \"ctrl-k\""));
        assert!(repaired.contains("edit_next = \"ctrl-j\""));
        assert!(repaired.contains(&format!(
            "{INVALID_OPTION_COMMENT}\n# history_previous = \"ctrl-k\""
        )));
        assert!(repaired.contains(&format!(
            "{INVALID_OPTION_COMMENT}\n# history_next = \"ctrl-j\""
        )));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn save_display_preserves_server_and_comments_invalid_options() {
        let path = env::temp_dir().join(format!(
            "axon-tui-test-{}-save-server-config.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let config_with_inline_comment =
            DEFAULT_CONFIG.replacen("input_lines = 1", "input_lines = 1 # keep height", 1);
        let original = format!(
            "{config_with_inline_comment}\n\
             # keep this server comment\n\
             [server]\n\
             base_url = \"https://axon.example.com\"\n\
             bearer_token = \"secret-token\"\n\
             retired_option = \"keep me\"\n"
        );
        fs::write(&path, original).expect("write config");

        TuiConfig::save_display(&path, 3, 25, 0).expect("save display");

        let saved = fs::read_to_string(&path).expect("read saved config");
        assert!(saved.contains("# keep this server comment"));
        assert!(saved.contains("base_url = \"https://axon.example.com\""));
        assert!(saved.contains("bearer_token = \"secret-token\""));
        assert!(saved.contains(&format!(
            "{INVALID_OPTION_COMMENT}\n# retired_option = \"keep me\""
        )));
        assert!(saved.contains("input_lines = 3"));
        assert!(saved.contains("input_lines = 3 # keep height"));
        assert!(saved.contains("accounts_panel_width = 25"));
        assert!(saved.contains("rooms_panel_width_adj = 0"));

        let config = TuiConfig::load_or_create_at(path.clone()).expect("reload saved config");
        assert_eq!(config.base_url.as_deref(), Some("https://axon.example.com"));
        assert_eq!(config.token.as_deref(), Some("secret-token"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn save_pinned_rooms_roundtrips_and_preserves_comments() {
        let path = env::temp_dir().join(format!(
            "axon-tui-test-{}-pinned-rooms-config.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let original = format!("# keep me\n{DEFAULT_CONFIG}");
        fs::write(&path, original).expect("write config");

        let pinned = vec![
            "00000000-0000-0000-0000-000000000000:!a:srv".to_owned(),
            "00000000-0000-0000-0000-000000000001:!b:srv".to_owned(),
        ];
        TuiConfig::save_pinned_rooms(&path, &pinned).expect("save pinned rooms");

        let saved = fs::read_to_string(&path).expect("read saved config");
        assert!(saved.contains("# keep me"));
        assert!(saved.contains("pinned_rooms = ["));

        let config = TuiConfig::load_or_create_at(path.clone()).expect("reload config");
        assert_eq!(config.display.pinned_rooms, pinned);

        // Saving an empty list clears the array.
        TuiConfig::save_pinned_rooms(&path, &[]).expect("clear pinned rooms");
        let config = TuiConfig::load_or_create_at(path.clone()).expect("reload cleared config");
        assert!(config.display.pinned_rooms.is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn save_room_view_roundtrips_and_preserves_comments() {
        let path = env::temp_dir().join(format!(
            "axon-tui-test-{}-room-view-config.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let original = format!("# keep me\n{DEFAULT_CONFIG}");
        fs::write(&path, original).expect("write config");

        TuiConfig::save_room_view(&path, "az", "dms").expect("save room view");
        let saved = fs::read_to_string(&path).expect("read saved config");
        assert!(saved.contains("# keep me"));
        assert!(saved.contains("room_sort = \"az\""));
        assert!(saved.contains("room_filter = \"dms\""));

        let config = TuiConfig::load_or_create_at(path.clone()).expect("reload config");
        assert_eq!(config.display.room_sort, "az");
        assert_eq!(config.display.room_filter, "dms");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn load_clamps_input_lines_to_max_input_lines() {
        let path = env::temp_dir().join(format!(
            "axon-tui-test-{}-input-line-bounds-config.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let original = DEFAULT_CONFIG
            .replacen("input_lines = 1", "input_lines = 5", 1)
            .replacen("max_input_lines = 10", "max_input_lines = 3", 1);
        fs::write(&path, original).expect("write config");

        let config = TuiConfig::load_or_create_at(path.clone()).expect("load config");

        assert_eq!(config.display.input_lines, 3);
        assert_eq!(config.display.max_input_lines, Some(3));
        let repaired = fs::read_to_string(&path).expect("read repaired config");
        assert!(repaired.contains("input_lines = 3"));
        assert!(repaired.contains("max_input_lines = 3"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rewrite_comments_out_invalid_tables_without_deleting_them() {
        let path = env::temp_dir().join(format!(
            "axon-tui-test-{}-invalid-table-config.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let original = format!(
            "{DEFAULT_CONFIG}\n\
             [retired]\n\
             first = \"one\"\n\
             second = 2\n"
        );
        fs::write(&path, original).expect("write config");

        TuiConfig::load_or_create_at(path.clone()).expect("load and rewrite config");

        let saved = fs::read_to_string(&path).expect("read rewritten config");
        assert!(saved.contains(&format!("{INVALID_OPTION_COMMENT}\n# [retired]")));
        assert!(saved.contains(&format!("{INVALID_OPTION_COMMENT}\n# first = \"one\"")));
        assert!(saved.contains(&format!("{INVALID_OPTION_COMMENT}\n# second = 2")));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn rewrite_comments_every_line_of_multiline_invalid_option() {
        let path = env::temp_dir().join(format!(
            "axon-tui-test-{}-invalid-multiline-config.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let original = format!(
            "{DEFAULT_CONFIG}\n\
             [server]\n\
             retired_hosts = [\n\
               \"one.example.com\",\n\
               \"two.example.com\",\n\
             ]\n"
        );
        fs::write(&path, original).expect("write config");

        TuiConfig::load_or_create_at(path.clone()).expect("load and rewrite config");

        let saved = fs::read_to_string(&path).expect("read rewritten config");
        for line in [
            "retired_hosts = [",
            "\"one.example.com\",",
            "\"two.example.com\",",
            "]",
        ] {
            assert!(
                saved.contains(&format!("{INVALID_OPTION_COMMENT}\n# {line}")),
                "missing commented invalid line: {line}"
            );
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn preserves_user_comments_when_config_is_complete() {
        let path = env::temp_dir().join(format!(
            "axon-tui-test-{}-comments-config.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let original = format!(
            "# my personal config\n# do not overwrite this!\n{}",
            DEFAULT_CONFIG
        );
        fs::write(&path, &original).expect("write config with comments");

        TuiConfig::load_or_create_at(path.clone()).expect("load config with comments");

        assert_eq!(
            fs::read_to_string(&path).expect("read config"),
            original,
            "user comments must not be discarded"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn parse_error_does_not_overwrite_existing_config() {
        let path = env::temp_dir().join(format!(
            "axon-tui-test-{}-bad-config.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let original = r#"[display]
input_lines = "one"
"#;
        fs::write(&path, original).expect("write invalid config");

        let err = TuiConfig::load_or_create_at(path.clone()).expect_err("invalid TOML type");

        assert!(matches!(err, ConfigError::Toml(_)));
        assert_eq!(
            fs::read_to_string(&path).expect("read invalid config"),
            original
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn save_display_survives_inline_table_display_section() {
        let path = env::temp_dir().join(format!(
            "axon-tui-test-{}-inline-table-display-config.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let original = DEFAULT_CONFIG.replacen(
            "[display]\ndebug = false\nshow_state_events = false\nmessage_density = \"normal\"\ninput_lines = 1\nconfirm_logout = true\nsearch_wrap = true",
            "display = { input_lines = 2 }",
            1,
        );
        fs::write(&path, &original).expect("write config with inline display table");

        TuiConfig::save_display(&path, 3, 25, 0).expect("save display with inline table");

        let saved = fs::read_to_string(&path).expect("read saved config");
        assert!(saved.contains("input_lines = 3"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn parses_message_density() {
        assert_eq!(
            parse_message_density("display.message_density", "Normal").unwrap(),
            MessageDensity::Normal
        );
        assert_eq!(
            parse_message_density("display.message_density", "dense").unwrap(),
            MessageDensity::Dense
        );
        assert!(parse_message_density("display.message_density", "unknown").is_err());
    }

    #[test]
    fn parses_color_names() {
        assert_eq!(
            parse_color("colors.status", "light-cyan").unwrap(),
            Color::LightCyan
        );
        assert_eq!(
            parse_color("colors.status", "dark_gray").unwrap(),
            Color::DarkGray
        );
    }

    #[test]
    fn selected_line_highlight_defaults_off_and_uses_configured_background() {
        let config = TuiConfig::test_default();
        assert!(!config.display.highlight_selected_line);
        assert_eq!(config.colors.selection_background, Color::DarkGray);

        let raw = RawConfig::load_with_defaults(
            r#"[colors]
selection_background = "blue"

[display]
highlight_selected_line = true
"#,
        )
        .expect("custom config parses")
        .0;
        let colors = raw.colors.into_color_scheme().expect("colors parse");
        let display = raw.display.into_display_options().expect("display parses");
        assert!(display.highlight_selected_line);
        assert_eq!(colors.selection_background, Color::Blue);
    }
}
