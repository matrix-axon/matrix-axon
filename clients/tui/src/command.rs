#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Login {
        username: Option<String>,
        password: Option<String>,
        /// Optional homeserver base URL override (the inline third argument).
        /// When `None`, Axon resolves the homeserver from the Matrix ID.
        homeserver: Option<String>,
    },
    Logout(Option<String>),
    Recover(Option<String>),
    /// `/backup enable [user]` — originate or kick megolm key backup.
    BackupEnable(Option<String>),
    Delete(Option<String>),
    Room(String),
    /// /pin [room] — pin the given room (or the selected room) to the top.
    Pin(Option<String>),
    /// /unpin [room] — unpin the given room (or the selected room).
    Unpin(Option<String>),
    /// /filter [all|dms|groups|unread|fav|<text>] — set the room-list filter
    /// (ADR 0042). Empty argument clears to all rooms.
    Filter(String),
    /// /sort [recent|oldest|az|za] — set the room-list sort (ADR 0042).
    Sort(String),
    Account(String),
    Status,
    Event(String),
    Whoami,
    Whereami,
    Search(crate::search::SearchCommandInput),
    React(Option<String>),
    Unreact,
    Reply,
    Thread,
    UnreadThreads,
    /// Start an outgoing SAS verification. The argument is either a device ID
    /// (self-verification, ADR 0028) or a `@user:server` (cross-user, ADR 0040).
    Verify(Option<String>),
    /// Inspect the per-event verification bundle for an event ID (M7c).
    Bundle(String),
    Help,
    Shortcuts,
    Refresh,
    SaveConfig,
    EditConfig,
    Quit,
    Send(String),
    /// /send <path> [caption] — upload a local file and send it to the current
    /// room as a media message.
    SendMedia {
        path: String,
        caption: Option<String>,
    },
    /// /html <raw-html> — send HTML literally as formatted_body
    SendHtml(String),
    /// /literal <text> — send as plaintext, skip markdown auto-conversion
    SendLiteral(String),
    /// /rainbow <text> — send text with each character in a cycling rainbow color
    Rainbow(String),
    /// /spoiler [reason |] text — send text hidden behind a spoiler warning
    Spoiler {
        reason: Option<String>,
        text: String,
    },
    /// /jump <date> — navigate the current room's timeline to the given date.
    /// The i64 is a Unix timestamp in milliseconds.
    JumpToDate(i64),
    /// /top — jump to the earliest message the Axon server has for the current room.
    JumpToTop,
    /// /leave or /part — leave the selected room.
    Leave,
    /// /forget [room] — forget the selected or named left/banned room.
    Forget(Option<String>),
    /// /invite <user> — invite a user to the selected room.
    Invite(String),
    /// /kick <user> [reason] — kick a user from the selected room.
    Kick(UserReasonCommand),
    /// /ban <user> [reason] — ban a user from the selected room.
    Ban(UserReasonCommand),
    /// /unban <user> [reason] — unban a user from the selected room.
    Unban(UserReasonCommand),
    Invalid(String),
    ApiUnsupported(String),
    Unknown(String),
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserReasonCommand {
    pub user_id: String,
    pub reason: Option<String>,
}

#[derive(Clone, Copy)]
pub(crate) struct SlashCommand {
    pub(crate) name: &'static str,
    pub(crate) takes_argument: bool,
    api_supported: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct HelpCommand {
    pub(crate) label: &'static str,
    pub(crate) insert_text: &'static str,
    pub(crate) description: &'static str,
}

impl SlashCommand {
    const fn supported(name: &'static str, takes_argument: bool) -> Self {
        Self {
            name,
            takes_argument,
            api_supported: true,
        }
    }

    const fn api_unsupported(name: &'static str, takes_argument: bool) -> Self {
        Self {
            name,
            takes_argument,
            api_supported: false,
        }
    }
}

pub(crate) const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand::supported("/login", true),
    SlashCommand::supported("/logout", true),
    SlashCommand::supported("/recover", true),
    SlashCommand::supported("/backup", true),
    SlashCommand::supported("/delete", true),
    SlashCommand::supported("/room", true),
    SlashCommand::supported("/switch", true),
    SlashCommand::supported("/pin", true),
    SlashCommand::supported("/unpin", true),
    SlashCommand::supported("/filter", true),
    SlashCommand::supported("/sort", true),
    SlashCommand::supported("/account", true),
    SlashCommand::supported("/status", false),
    SlashCommand::supported("/event", true),
    SlashCommand::supported("/whoami", false),
    SlashCommand::supported("/whereami", false),
    SlashCommand::supported("/search", true),
    SlashCommand::supported("/react", true),
    SlashCommand::supported("/unreact", false),
    SlashCommand::supported("/reply", false),
    SlashCommand::supported("/thread", false),
    SlashCommand::supported("/unreadthreads", false),
    SlashCommand::supported("/ut", false),
    SlashCommand::supported("/verify", true),
    SlashCommand::supported("/bundle", true),
    SlashCommand::supported("/help", false),
    SlashCommand::supported("/shortcuts", false),
    SlashCommand::supported("/refresh", false),
    SlashCommand::supported("/rooms", false),
    SlashCommand::supported("/saveconfig", false),
    SlashCommand::supported("/editconfig", false),
    SlashCommand::supported("/quit", false),
    SlashCommand::supported("/send", true),
    SlashCommand::supported("/html", true),
    SlashCommand::supported("/literal", true),
    SlashCommand::supported("/rainbow", true),
    SlashCommand::supported("/spoiler", true),
    SlashCommand::supported("/jump", true),
    SlashCommand::supported("/top", false),
    SlashCommand::supported("/leave", false),
    SlashCommand::supported("/part", false),
    SlashCommand::supported("/forget", true),
    SlashCommand::supported("/invite", true),
    SlashCommand::supported("/kick", true),
    SlashCommand::supported("/ban", true),
    SlashCommand::supported("/unban", true),
    SlashCommand::api_unsupported("/join", true),
];

/// Group boundaries for the help popup: `(start_index, section_title)`.
/// Each entry marks the first `HELP_COMMANDS` index belonging to that group.
pub(crate) const HELP_COMMAND_GROUPS: &[(usize, &str)] = &[
    (0, "Messaging"),
    (8, "Navigation"),
    (14, "Account management"),
    (19, "Information"),
    (23, "Message actions"),
    (28, "Room actions"),
    (35, "Verification"),
    (37, "System"),
    (43, "Pending"),
];

pub(crate) const HELP_COMMANDS: &[HelpCommand] = &[
    // ── Messaging ────────────────────────────────────────────────────────────
    HelpCommand {
        label: "plain text",
        insert_text: "",
        description: "send a message to the current room",
    },
    HelpCommand {
        label: "Alt+Enter",
        insert_text: "",
        description: "insert a line break for a multi-line message",
    },
    HelpCommand {
        label: "//<text>",
        insert_text: "//",
        description: "send a message beginning with a literal /",
    },
    HelpCommand {
        label: "/html <html>",
        insert_text: "/html ",
        description: "send raw HTML as a formatted message",
    },
    HelpCommand {
        label: "/literal <text>",
        insert_text: "/literal ",
        description: "send text as plaintext, skipping markdown auto-conversion",
    },
    HelpCommand {
        label: "/rainbow <text>",
        insert_text: "/rainbow ",
        description: "send text with each character colored in cycling rainbow hues",
    },
    HelpCommand {
        label: "/spoiler [reason |] <text>",
        insert_text: "/spoiler ",
        description: "send text as a spoiler; label optional reason before \" | \"",
    },
    HelpCommand {
        label: "/send <path> [caption]",
        insert_text: "/send ",
        description: "upload a local file, tab-complete or drag-and-drop to fill path",
    },
    // ── Navigation ───────────────────────────────────────────────────────────
    HelpCommand {
        label: "/room <room>, /switch <room>",
        insert_text: "/room ",
        description: "switch room by name, alias, ID, or number",
    },
    HelpCommand {
        label: "/pin [room], /unpin [room]",
        insert_text: "/pin ",
        description: "pin (or unpin) a room to the top of list; default current room",
    },
    HelpCommand {
        label: "/filter [all|dms|groups|unread|fav|<text>]",
        insert_text: "/filter ",
        description: "set the room-list filter",
    },
    HelpCommand {
        label: "/sort [recent|oldest|az|za]",
        insert_text: "/sort ",
        description: "set the room-list sort order",
    },
    HelpCommand {
        label: "/account <account>",
        insert_text: "/account ",
        description: "filter by account (user ID, localpart, number, or \"all\")",
    },
    HelpCommand {
        label: "/unreadthreads, /ut",
        insert_text: "/unreadthreads",
        description: "open the unread thread picker",
    },
    // ── Account management ───────────────────────────────────────────────────
    HelpCommand {
        label: "/login [user] [password] [homeserver]",
        insert_text: "/login ",
        description: "log into a Matrix account",
    },
    HelpCommand {
        label: "/logout [user]",
        insert_text: "/logout ",
        description: "log out an active account (retains messages)",
    },
    HelpCommand {
        label: "/recover [user]",
        insert_text: "/recover ",
        description: "import encryption keys for an active account",
    },
    HelpCommand {
        label: "/backup enable [user]",
        insert_text: "/backup enable ",
        description: "enable megolm key backup for a verified account",
    },
    HelpCommand {
        label: "/delete [user]",
        insert_text: "/delete ",
        description: "permanently delete an account and all its data",
    },
    // ── Information ──────────────────────────────────────────────────────────
    HelpCommand {
        label: "/status",
        insert_text: "/status",
        description: "server connectivity, accounts, and megolm backup state",
    },
    HelpCommand {
        label: "/event <id>",
        insert_text: "/event ",
        description: "show raw event JSON in status",
    },
    HelpCommand {
        label: "/whoami",
        insert_text: "/whoami",
        description: "show your Matrix ID and display name",
    },
    HelpCommand {
        label: "/whereami",
        insert_text: "/whereami",
        description: "show room information",
    },
    // ── Message actions ──────────────────────────────────────────────────────
    HelpCommand {
        label: "/search [field:value...] <query>",
        insert_text: "/search ",
        description: "search history; bare /search for interactive; /search ? for syntax",
    },
    HelpCommand {
        label: "/react [emoji]",
        insert_text: "/react ",
        description: "react to the selected or most recent message",
    },
    HelpCommand {
        label: "/unreact",
        insert_text: "/unreact",
        description: "withdraw one of your reactions",
    },
    HelpCommand {
        label: "/reply",
        insert_text: "/reply",
        description: "reply to the selected or most recent message",
    },
    HelpCommand {
        label: "/thread",
        insert_text: "/thread",
        description: "open the thread on, or start a thread from, the selected message",
    },
    // ── Room actions ────────────────────────────────────────────────────────
    HelpCommand {
        label: "/leave, /part",
        insert_text: "/leave",
        description: "leave the selected room after confirmation",
    },
    HelpCommand {
        label: "/forget [room]",
        insert_text: "/forget ",
        description: "forget the selected or named room after confirmation",
    },
    HelpCommand {
        label: "/invite <user>",
        insert_text: "/invite ",
        description: "invite a Matrix user to the selected room",
    },
    HelpCommand {
        label: "/kick <user> [reason]",
        insert_text: "/kick ",
        description: "kick a user from the selected room after confirmation",
    },
    HelpCommand {
        label: "/ban <user> [reason]",
        insert_text: "/ban ",
        description: "ban a user from the selected room after confirmation",
    },
    HelpCommand {
        label: "/unban <user> [reason]",
        insert_text: "/unban ",
        description: "unban a user from the selected room after confirmation",
    },
    HelpCommand {
        label: "/join <room>",
        insert_text: "/join ",
        description: "pending TUI-M19-2",
    },
    // ── Verification ─────────────────────────────────────────────────────────
    HelpCommand {
        label: "/verify <device_id|@user:server>",
        insert_text: "/verify ",
        description:
            "start emoji SAS verification of a device or another user (incoming requests open automatically)",
    },
    HelpCommand {
        label: "/bundle <event_id>",
        insert_text: "/bundle ",
        description: "show the sender-trust verification bundle for an event",
    },
    // ── System ───────────────────────────────────────────────────────────────
    HelpCommand {
        label: "/help, /?",
        insert_text: "/help",
        description: "show this help",
    },
    HelpCommand {
        label: "/shortcuts",
        insert_text: "/shortcuts",
        description: "show keyboard shortcuts",
    },
    HelpCommand {
        label: "/refresh, /rooms",
        insert_text: "/refresh",
        description: "refresh rooms and redraw the display",
    },
    HelpCommand {
        label: "/saveconfig",
        insert_text: "/saveconfig",
        description: "save current display settings (input lines, pane widths) to config file",
    },
    HelpCommand {
        label: "/editconfig",
        insert_text: "/editconfig",
        description: "open the config file in $EDITOR (nano/notepad if unset)",
    },
    HelpCommand {
        label: "/quit, /q",
        insert_text: "/quit",
        description: "quit",
    },
    // ── Pending ──────────────────────────────────────────────────────────────
    HelpCommand {
        label: "/jump <date>",
        insert_text: "/jump ",
        description: "jump to a date in the current room (YYYY-MM-DD, M/D/YYYY, or Jan 15 2025)",
    },
    HelpCommand {
        label: "/top",
        insert_text: "/top",
        description: "jump to the earliest available message in the current room",
    },
];

pub fn parse(input: &str) -> Command {
    let input = input.trim();
    if input.is_empty() {
        return Command::Empty;
    }
    if let Some(message) = input.strip_prefix("//") {
        return Command::Send(format!("/{message}"));
    }
    if !input.starts_with('/') {
        return Command::Send(input.to_owned());
    }

    let mut parts = input[1..].splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let arg = parts.next().unwrap_or_default().trim();
    match name {
        "login" => {
            // Positional: <user> <password> [homeserver]. The inline password is
            // a single token; a password with spaces is rejected here so it can be
            // typed at the hidden prompt (see the `/login` flow). The homeserver,
            // when present, is the third token and overrides Axon's resolution.
            let mut tokens = arg.split_whitespace();
            let username = tokens.next().map(str::to_owned);
            let password = tokens.next().map(str::to_owned);
            let homeserver = tokens.next().map(str::to_owned);
            if tokens.next().is_some() {
                return Command::Invalid(
                    "/login takes at most <user> <password> [homeserver]; for a password with \
                     spaces run `/login` (or `/login <user> [homeserver]`) and type it at the \
                     hidden prompt"
                        .to_owned(),
                );
            }
            Command::Login {
                username,
                password,
                homeserver,
            }
        }
        "logout" => Command::Logout((!arg.is_empty()).then(|| arg.to_owned())),
        "recover" => {
            let mut tokens = arg.split_whitespace();
            let target = tokens.next().map(str::to_owned);
            if tokens.next().is_some() {
                Command::Invalid(
                    "/recover takes at most one account target; the recovery key is entered at \
                     the hidden prompt"
                        .to_owned(),
                )
            } else {
                Command::Recover(target)
            }
        }
        "backup" => parse_backup_command(arg),
        "delete" => Command::Delete((!arg.is_empty()).then(|| arg.to_owned())),
        "room" | "switch" if !arg.is_empty() => Command::Room(arg.to_owned()),
        "room" | "switch" => {
            Command::Invalid("/room requires a room id, alias, name, or index".to_owned())
        }
        "pin" => Command::Pin((!arg.is_empty()).then(|| arg.to_owned())),
        "unpin" => Command::Unpin((!arg.is_empty()).then(|| arg.to_owned())),
        "filter" => Command::Filter(arg.to_owned()),
        "sort" => Command::Sort(arg.to_owned()),
        "account" if !arg.is_empty() => Command::Account(arg.to_owned()),
        "account" => Command::Invalid(
            "/account requires a user ID, localpart, number, or \"all\"".to_owned(),
        ),
        "status" => Command::Status,
        "event" if !arg.is_empty() => Command::Event(arg.to_owned()),
        "event" => Command::Invalid("/event requires an event id".to_owned()),
        "whoami" => Command::Whoami,
        "whereami" => Command::Whereami,
        "search" => Command::Search(crate::search::parse_search_command_arg(arg)),
        "react" => Command::React((!arg.is_empty()).then(|| arg.to_owned())),
        "unreact" => Command::Unreact,
        "reply" => Command::Reply,
        "thread" => Command::Thread,
        "unreadthreads" | "ut" => Command::UnreadThreads,
        "verify" => {
            let mut tokens = arg.split_whitespace();
            let target = tokens.next().map(str::to_owned);
            if tokens.next().is_some() {
                Command::Invalid("/verify takes at most one device id or @user:server".to_owned())
            } else {
                Command::Verify(target)
            }
        }
        "bundle" if !arg.is_empty() => Command::Bundle(arg.to_owned()),
        "bundle" => Command::Invalid("/bundle requires an event id".to_owned()),
        "help" | "?" => Command::Help,
        "shortcuts" => Command::Shortcuts,
        "refresh" | "rooms" => Command::Refresh,
        "saveconfig" => Command::SaveConfig,
        "editconfig" => Command::EditConfig,
        "quit" | "q" => Command::Quit,
        "send" if !arg.is_empty() => {
            let (path, caption) = parse_leading_path_token(arg);
            if path.is_empty() {
                Command::Invalid("/send requires a file path".to_owned())
            } else {
                Command::SendMedia { path, caption }
            }
        }
        "send" => Command::Invalid("/send requires a file path".to_owned()),
        "html" if !arg.is_empty() => Command::SendHtml(arg.to_owned()),
        "html" => Command::Invalid("/html requires HTML content to send".to_owned()),
        "literal" if !arg.is_empty() => Command::SendLiteral(arg.to_owned()),
        "literal" => Command::Invalid("/literal requires text to send".to_owned()),
        "rainbow" if !arg.is_empty() => Command::Rainbow(arg.to_owned()),
        "rainbow" => Command::Invalid("/rainbow requires text to send".to_owned()),
        "spoiler" if !arg.is_empty() => {
            if let Some((reason, text)) = arg.split_once(" | ") {
                let reason = reason.trim();
                Command::Spoiler {
                    reason: (!reason.is_empty()).then(|| reason.to_owned()),
                    text: text.to_owned(),
                }
            } else {
                Command::Spoiler {
                    reason: None,
                    text: arg.to_owned(),
                }
            }
        }
        "spoiler" => Command::Invalid("/spoiler requires text to send".to_owned()),
        "jump" if !arg.is_empty() => match parse_date_to_ms(arg) {
            Some(ts) => Command::JumpToDate(ts),
            None => Command::Invalid(format!(
                "/jump: unrecognized date \"{arg}\" — try YYYY-MM-DD, M/D/YYYY, or \"Jan 15 2025\""
            )),
        },
        "jump" => Command::Invalid(
            "/jump requires a date (YYYY-MM-DD, M/D/YYYY, or \"January 15 2025\")".to_owned(),
        ),
        "top" => Command::JumpToTop,
        "leave" | "part" => Command::Leave,
        "forget" => Command::Forget((!arg.trim().is_empty()).then(|| arg.trim().to_owned())),
        "invite" if !arg.trim().is_empty() => Command::Invite(arg.trim().to_owned()),
        "invite" => Command::Invalid("/invite requires a Matrix user id".to_owned()),
        "kick" => {
            parse_user_reason_command(arg, "/kick").map_or_else(Command::Invalid, Command::Kick)
        }
        "ban" => parse_user_reason_command(arg, "/ban").map_or_else(Command::Invalid, Command::Ban),
        "unban" => {
            parse_user_reason_command(arg, "/unban").map_or_else(Command::Invalid, Command::Unban)
        }
        other => {
            let command_name = format!("/{other}");
            if SLASH_COMMANDS
                .iter()
                .any(|command| command.name == command_name && !command.api_supported)
            {
                Command::ApiUnsupported(format!(
                    "{command_name} is not supported by the current Axon API"
                ))
            } else {
                Command::Unknown(format!("unknown command: {command_name}"))
            }
        }
    }
}

fn parse_backup_command(arg: &str) -> Command {
    let mut tokens = arg.split_whitespace();
    match tokens.next() {
        Some("enable") => {
            let target = tokens.next().map(str::to_owned);
            if tokens.next().is_some() {
                Command::Invalid(
                    "/backup enable takes at most one account target; the recovery key is \
                     entered at the hidden prompt"
                        .to_owned(),
                )
            } else {
                Command::BackupEnable(target)
            }
        }
        Some(other) => Command::Invalid(format!(
            "unknown /backup subcommand: {other}; try /backup enable"
        )),
        None => Command::Invalid("usage: /backup enable [user]".to_owned()),
    }
}

fn parse_user_reason_command(arg: &str, command: &str) -> Result<UserReasonCommand, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Err(format!("{command} requires a Matrix user id"));
    }
    let mut parts = arg.splitn(2, char::is_whitespace);
    let user_id = parts.next().unwrap_or_default().trim();
    if user_id.is_empty() {
        return Err(format!("{command} requires a Matrix user id"));
    }
    let reason = parts
        .next()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(str::to_owned);
    Ok(UserReasonCommand {
        user_id: user_id.to_owned(),
        reason,
    })
}

/// Scan an *unquoted* `/send` argument for its leading path token:
/// characters up to the first unescaped whitespace, with `\ ` / `\\` / `\'` /
/// `\"` unescaped. Returns the unescaped path plus the byte offset in `arg`
/// where caption text begins — `None` when the whole string was consumed as
/// the path (no unescaped whitespace seen yet, i.e. still mid-token). The
/// sole tokenizing primitive for the unquoted case, shared by
/// [`parse_leading_path_token`] and completion's
/// [`send_argument_still_in_path_token`] so they can never disagree on where
/// the path ends.
fn scan_unquoted_path_token(arg: &str) -> (String, Option<usize>) {
    let mut path = String::new();
    let mut chars = arg.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch == '\\' {
            if let Some(&(_, next_ch)) = chars.peek() {
                if matches!(next_ch, ' ' | '\\' | '\'' | '"') {
                    path.push(next_ch);
                    chars.next();
                    continue;
                }
            }
            path.push(ch);
            continue;
        }
        if ch.is_whitespace() {
            return (path, Some(idx + ch.len_utf8()));
        }
        path.push(ch);
    }
    (path, None)
}

/// Split a `/send` argument into its leading path token and an optional
/// caption. The path is either a `'...'`/`"..."`-quoted run, or characters up
/// to the first unescaped whitespace with `\ ` / `\\` / `\'` / `\"`
/// unescaped — matching what terminal emulators produce when a dropped
/// file's path is typed out (quoted, or backslash-escaped). Everything after
/// the path, trimmed, becomes the caption. Shared with tab completion
/// (`app/completion.rs`) so parsing and completion agree on where the path
/// ends.
pub(crate) fn parse_leading_path_token(arg: &str) -> (String, Option<String>) {
    if let Some(quote) = arg.chars().next().filter(|&ch| ch == '\'' || ch == '"') {
        let rest = &arg[quote.len_utf8()..];
        return match rest.find(quote) {
            Some(end) => {
                let path = rest[..end].to_owned();
                let caption = rest[end + quote.len_utf8()..].trim();
                (path, (!caption.is_empty()).then(|| caption.to_owned()))
            }
            // No closing quote: treat the rest of the line as the path rather
            // than silently truncating a real filename.
            None => (rest.to_owned(), None),
        };
    }
    let (path, caption_start) = scan_unquoted_path_token(arg);
    let caption = caption_start
        .and_then(|start| arg.get(start..))
        .unwrap_or("")
        .trim();
    (path, (!caption.is_empty()).then(|| caption.to_owned()))
}

/// Whether a `/send` argument typed so far is still mid-path-token — i.e.
/// filename Tab completion should apply rather than treating the rest as
/// caption text. An unclosed quote, or an unquoted run with no unescaped
/// whitespace yet, are both "still typing the path". Built on the exact same
/// tokenizing primitives [`parse_leading_path_token`] uses, so completion and
/// submission can never disagree on where the path ends (the bug this
/// replaced: a naive `contains(char::is_whitespace)` check treated a
/// backslash-escaped space, e.g. `My\ File`, as if the path were already
/// finished).
pub(crate) fn send_argument_still_in_path_token(arg: &str) -> bool {
    if let Some(quote) = arg.chars().next().filter(|&ch| ch == '\'' || ch == '"') {
        let rest = &arg[quote.len_utf8()..];
        return rest.find(quote).is_none();
    }
    scan_unquoted_path_token(arg).1.is_none()
}

/// Parse a human-readable date string into a Unix timestamp in milliseconds
/// (midnight UTC for date-only forms). Returns `None` for unrecognized formats.
///
/// Supported formats:
///   YYYY-MM-DD          e.g. 2025-01-15
///   YYYY-MM-DD HH:MM    e.g. 2025-01-15 10:30
///   M/D/YYYY            e.g. 1/15/2025  or  01/15/2025
///   Mon D YYYY          e.g. Jan 15 2025  or  January 15 2025
///   Mon D, YYYY         e.g. January 15, 2025
pub(crate) fn parse_date_to_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Some(ts) = try_parse_iso(s) {
        return Some(ts);
    }
    if let Some(ts) = try_parse_us_slash(s) {
        return Some(ts);
    }
    if let Some(ts) = try_parse_named_month(s) {
        return Some(ts);
    }
    None
}

/// YYYY-MM-DD or YYYY-MM-DD HH:MM
fn try_parse_iso(s: &str) -> Option<i64> {
    if s.len() < 10 || s.as_bytes()[4] != b'-' || s.as_bytes()[7] != b'-' {
        return None;
    }
    let (y, m, d) = (
        s[0..4].parse::<i64>().ok()?,
        s[5..7].parse::<u32>().ok()?,
        s[8..10].parse::<u32>().ok()?,
    );
    let (h, min) = if s.len() >= 16 && s.as_bytes()[10] == b' ' {
        let rest = &s[11..];
        if rest.len() >= 5 && rest.as_bytes()[2] == b':' {
            (
                rest[0..2].parse::<u32>().ok()?,
                rest[3..5].parse::<u32>().ok()?,
            )
        } else {
            return None;
        }
    } else if s.len() == 10 {
        (0, 0)
    } else {
        return None;
    };
    ymd_hm_to_ms(y, m, d, h, min)
}

/// M/D/YYYY or MM/DD/YYYY
fn try_parse_us_slash(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.splitn(3, '/').collect();
    if parts.len() != 3 {
        return None;
    }
    let m = parts[0].parse::<u32>().ok()?;
    let d = parts[1].parse::<u32>().ok()?;
    let y = parts[2].trim().parse::<i64>().ok()?;
    ymd_hm_to_ms(y, m, d, 0, 0)
}

/// "January 15 2025", "Jan 15 2025", "January 15, 2025", "Jan 15, 2025"
fn try_parse_named_month(s: &str) -> Option<i64> {
    // Split on whitespace; allow an optional comma after the day token.
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.len() < 3 {
        return None;
    }
    let m = month_name_to_number(tokens[0])?;
    let day_str = tokens[1].trim_end_matches(',');
    let d = day_str.parse::<u32>().ok()?;
    let y = tokens[2].trim_end_matches(',').parse::<i64>().ok()?;
    ymd_hm_to_ms(y, m, d, 0, 0)
}

fn month_name_to_number(name: &str) -> Option<u32> {
    let prefix = name
        .chars()
        .take(3)
        .collect::<String>()
        .to_ascii_lowercase();
    match prefix.as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

pub(crate) fn ymd_hm_to_ms(y: i64, m: u32, d: u32, h: u32, min: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) || h > 23 || min > 59 {
        return None;
    }
    let days = days_since_epoch(y, m, d)?;
    let secs = days * 86400 + h as i64 * 3600 + min as i64 * 60;
    Some(secs * 1000)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Days from 1970-01-01 to the given date using the Gregorian civil-calendar algorithm.
fn days_since_epoch(y: i64, m: u32, d: u32) -> Option<i64> {
    // Shift Jan/Feb to previous year so leap-day math is at the end of the year.
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    // Days from a reference era epoch to the given date.
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400); // year-of-era [0, 399]
    let doy = (153 * m as i64 + 2) / 5 + d as i64 - 1; // day-of-year [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // day-of-era [0, 146096]
    let civil_epoch_days = era * 146097 + doe - 719468; // days since Unix epoch
    Some(civil_epoch_days)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_send_media_forms() {
        assert_eq!(
            parse("/send photo.png"),
            Command::SendMedia {
                path: "photo.png".to_owned(),
                caption: None,
            }
        );
        assert_eq!(
            parse("/send photo.png sunset over the bay"),
            Command::SendMedia {
                path: "photo.png".to_owned(),
                caption: Some("sunset over the bay".to_owned()),
            }
        );
        assert_eq!(
            parse("/send '/home/user/My File.png' here's the sunset"),
            Command::SendMedia {
                path: "/home/user/My File.png".to_owned(),
                caption: Some("here's the sunset".to_owned()),
            }
        );
        assert_eq!(
            parse("/send \"/home/user/My File.png\""),
            Command::SendMedia {
                path: "/home/user/My File.png".to_owned(),
                caption: None,
            }
        );
        assert_eq!(
            parse("/send /home/user/My\\ File.png caption text"),
            Command::SendMedia {
                path: "/home/user/My File.png".to_owned(),
                caption: Some("caption text".to_owned()),
            }
        );
        assert_eq!(
            parse("/send"),
            Command::Invalid("/send requires a file path".to_owned())
        );
    }

    #[test]
    fn send_argument_still_in_path_token_matches_the_shared_tokenizer() {
        assert!(send_argument_still_in_path_token("photo.png"));
        assert!(!send_argument_still_in_path_token("photo.png caption"));
        // A backslash-escaped space is still part of the path, not the start
        // of caption text (this is the bug the shared tokenizer fixed: a
        // naive whitespace check would wrongly say the path is finished).
        assert!(send_argument_still_in_path_token("My\\ File"));
        assert!(!send_argument_still_in_path_token("My\\ File caption"));
        // Still inside an unterminated quote.
        assert!(send_argument_still_in_path_token("'my file"));
        assert!(!send_argument_still_in_path_token("'my file' caption"));
    }

    #[test]
    fn leading_path_token_handles_quoting_and_escapes() {
        assert_eq!(
            parse_leading_path_token("photo.png"),
            ("photo.png".to_owned(), None)
        );
        assert_eq!(
            parse_leading_path_token("'a b.png' caption"),
            ("a b.png".to_owned(), Some("caption".to_owned()))
        );
        assert_eq!(
            parse_leading_path_token("a\\ b.png"),
            ("a b.png".to_owned(), None)
        );
        assert_eq!(
            parse_leading_path_token("'unterminated quote here"),
            ("unterminated quote here".to_owned(), None)
        );
    }

    #[test]
    fn parses_room() {
        assert_eq!(parse("/room 2"), Command::Room("2".to_owned()));
        assert_eq!(
            parse("/room #room:localhost"),
            Command::Room("#room:localhost".to_owned())
        );
        assert_eq!(parse("/switch 2"), Command::Room("2".to_owned()));
    }

    #[test]
    fn parses_pin_and_unpin() {
        assert_eq!(parse("/pin"), Command::Pin(None));
        assert_eq!(parse("/pin 2"), Command::Pin(Some("2".to_owned())));
        assert_eq!(
            parse("/pin #room:localhost"),
            Command::Pin(Some("#room:localhost".to_owned()))
        );
        assert_eq!(parse("/unpin"), Command::Unpin(None));
        assert_eq!(parse("/unpin 2"), Command::Unpin(Some("2".to_owned())));
    }

    #[test]
    fn parses_login_forms() {
        assert_eq!(
            parse("/login"),
            Command::Login {
                username: None,
                password: None,
                homeserver: None,
            }
        );
        assert_eq!(
            parse("/login @me:example.com"),
            Command::Login {
                username: Some("@me:example.com".to_owned()),
                password: None,
                homeserver: None,
            }
        );
        assert_eq!(
            parse("/login @me:example.com hunter2"),
            Command::Login {
                username: Some("@me:example.com".to_owned()),
                password: Some("hunter2".to_owned()),
                homeserver: None,
            }
        );
    }

    #[test]
    fn parses_login_with_homeserver_override() {
        assert_eq!(
            parse("/login @me:example.com hunter2 homeserver.example.org"),
            Command::Login {
                username: Some("@me:example.com".to_owned()),
                password: Some("hunter2".to_owned()),
                homeserver: Some("homeserver.example.org".to_owned()),
            }
        );
    }

    #[test]
    fn rejects_inline_password_with_spaces() {
        // The single-token inline password means extra tokens are a mistake
        // (most likely a space in the password) — steer to the hidden prompt.
        assert!(matches!(
            parse("/login @me:example.com a password with spaces"),
            Command::Invalid(_)
        ));
    }

    #[test]
    fn parses_logout_forms() {
        assert_eq!(parse("/logout"), Command::Logout(None));
        assert_eq!(
            parse("/logout @me:example.com"),
            Command::Logout(Some("@me:example.com".to_owned()))
        );
        assert_eq!(parse("/logout me"), Command::Logout(Some("me".to_owned())));
    }

    #[test]
    fn parses_delete_forms() {
        assert_eq!(parse("/delete"), Command::Delete(None));
        assert_eq!(
            parse("/delete @me:example.com"),
            Command::Delete(Some("@me:example.com".to_owned()))
        );
        assert_eq!(parse("/delete me"), Command::Delete(Some("me".to_owned())));
    }

    #[test]
    fn parses_recover_forms_and_rejects_inline_keys() {
        assert_eq!(parse("/recover"), Command::Recover(None));
        assert_eq!(
            parse("/recover @me:example.com"),
            Command::Recover(Some("@me:example.com".to_owned()))
        );
        assert!(matches!(
            parse("/recover @me:example.com inline-key"),
            Command::Invalid(message) if message.contains("hidden prompt")
        ));
    }

    #[test]
    fn parses_backup_enable_forms_and_rejects_inline_keys() {
        assert_eq!(parse("/backup enable"), Command::BackupEnable(None));
        assert_eq!(
            parse("/backup enable @me:example.com"),
            Command::BackupEnable(Some("@me:example.com".to_owned()))
        );
        assert!(matches!(
            parse("/backup"),
            Command::Invalid(message) if message.contains("/backup enable")
        ));
        assert!(matches!(
            parse("/backup status"),
            Command::Invalid(message) if message.contains("unknown /backup subcommand")
        ));
        assert!(matches!(
            parse("/backup enable @me:example.com inline-key"),
            Command::Invalid(message) if message.contains("hidden prompt")
        ));
    }

    #[test]
    fn parses_verify_forms() {
        assert_eq!(parse("/verify"), Command::Verify(None));
        assert_eq!(
            parse("/verify ABCDEF"),
            Command::Verify(Some("ABCDEF".to_owned()))
        );
        assert_eq!(
            parse("/verify @bob:example.org"),
            Command::Verify(Some("@bob:example.org".to_owned()))
        );
        assert!(matches!(parse("/verify a b"), Command::Invalid(_)));
    }

    #[test]
    fn parses_bundle_forms() {
        assert_eq!(
            parse("/bundle $event:localhost"),
            Command::Bundle("$event:localhost".to_owned())
        );
        assert!(matches!(parse("/bundle"), Command::Invalid(_)));
    }

    #[test]
    fn parses_room_membership_actions() {
        assert_eq!(parse("/leave"), Command::Leave);
        assert_eq!(parse("/part"), Command::Leave);
        assert_eq!(parse("/forget"), Command::Forget(None));
        assert_eq!(
            parse("/forget #old:example.org"),
            Command::Forget(Some("#old:example.org".to_owned()))
        );
        assert_eq!(
            parse("/invite @bob:example.org"),
            Command::Invite("@bob:example.org".to_owned())
        );
        assert!(matches!(parse("/invite"), Command::Invalid(_)));
        assert_eq!(
            parse("/kick @bob:example.org off topic"),
            Command::Kick(UserReasonCommand {
                user_id: "@bob:example.org".to_owned(),
                reason: Some("off topic".to_owned()),
            })
        );
        assert_eq!(
            parse("/ban @bob:example.org"),
            Command::Ban(UserReasonCommand {
                user_id: "@bob:example.org".to_owned(),
                reason: None,
            })
        );
        assert_eq!(
            parse("/unban @bob:example.org time served"),
            Command::Unban(UserReasonCommand {
                user_id: "@bob:example.org".to_owned(),
                reason: Some("time served".to_owned()),
            })
        );
    }

    #[test]
    fn parses_quit_aliases() {
        assert_eq!(parse("/quit"), Command::Quit);
        assert_eq!(parse("/q"), Command::Quit);
    }

    #[test]
    fn parses_help() {
        assert_eq!(parse("/help"), Command::Help);
        assert_eq!(parse("/?"), Command::Help);
    }

    #[test]
    fn help_lists_filter_and_sort_commands() {
        let labels: Vec<&str> = HELP_COMMANDS.iter().map(|command| command.label).collect();

        assert!(labels.iter().any(|label| label.starts_with("/filter ")));
        assert!(labels.iter().any(|label| label.starts_with("/sort ")));
    }

    #[test]
    fn parses_shortcuts() {
        assert_eq!(parse("/shortcuts"), Command::Shortcuts);
    }

    #[test]
    fn parses_unread_threads() {
        assert_eq!(parse("/unreadthreads"), Command::UnreadThreads);
        assert_eq!(parse("/ut"), Command::UnreadThreads);
    }

    #[test]
    fn parses_refresh() {
        assert_eq!(parse("/refresh"), Command::Refresh);
        assert_eq!(parse("/rooms"), Command::Refresh);
    }

    #[test]
    fn parses_whoami() {
        assert_eq!(parse("/whoami"), Command::Whoami);
    }

    #[test]
    fn parses_whereami() {
        assert_eq!(parse("/whereami"), Command::Whereami);
    }

    #[test]
    fn parses_search_forms() {
        assert_eq!(
            parse("/search"),
            Command::Search(crate::search::SearchCommandInput::OpenForm)
        );
        assert_eq!(
            parse("/search ?"),
            Command::Search(crate::search::SearchCommandInput::Help)
        );
        assert_eq!(
            parse("/search help"),
            Command::Search(crate::search::SearchCommandInput::Run("help".to_owned()))
        );
        assert_eq!(
            parse("/search room:* backup key"),
            Command::Search(crate::search::SearchCommandInput::Run(
                "room:* backup key".to_owned()
            ))
        );
    }

    #[test]
    fn parses_message_action_commands() {
        assert_eq!(parse("/react"), Command::React(None));
        assert_eq!(parse("/react +1"), Command::React(Some("+1".to_owned())));
        assert_eq!(parse("/react 🚀"), Command::React(Some("🚀".to_owned())));
        assert_eq!(parse("/unreact"), Command::Unreact);
        assert_eq!(parse("/reply"), Command::Reply);
        assert_eq!(parse("/thread"), Command::Thread);
    }

    #[test]
    fn parses_plain_text_as_send() {
        assert_eq!(parse("hello"), Command::Send("hello".to_owned()));
        assert_eq!(
            parse("  hello world  "),
            Command::Send("hello world".to_owned())
        );
    }

    #[test]
    fn parses_double_slash_as_literal_leading_slash() {
        assert_eq!(parse("//help"), Command::Send("/help".to_owned()));
        assert_eq!(parse("///help"), Command::Send("//help".to_owned()));
        assert_eq!(parse("//"), Command::Send("/".to_owned()));
        assert_eq!(parse("  //help  "), Command::Send("/help".to_owned()));
    }

    #[test]
    fn reports_missing_arguments() {
        assert_eq!(
            parse("/room"),
            Command::Invalid("/room requires a room id, alias, name, or index".to_owned())
        );
        assert_eq!(
            parse("/event"),
            Command::Invalid("/event requires an event id".to_owned())
        );
    }

    #[test]
    fn reports_known_api_unsupported_commands() {
        assert_eq!(
            parse("/join #room:localhost"),
            Command::ApiUnsupported("/join is not supported by the current Axon API".to_owned())
        );
    }

    #[test]
    fn reports_unknown_commands() {
        assert_eq!(
            parse("/frobnicate"),
            Command::Unknown("unknown command: /frobnicate".to_owned())
        );
    }

    // ── /jump date parsing ──────────────────────────────────────────────────

    #[test]
    fn parse_date_iso() {
        // 2025-01-15 00:00:00 UTC = 1736899200000 ms
        assert_eq!(parse_date_to_ms("2025-01-15"), Some(1_736_899_200_000));
    }

    #[test]
    fn parse_date_iso_with_time() {
        // 2025-01-15 10:30:00 UTC = 1736899200000 + 10*3600 + 30*60 = +37800s
        assert_eq!(
            parse_date_to_ms("2025-01-15 10:30"),
            Some(1_736_899_200_000 + 37_800_000)
        );
    }

    #[test]
    fn parse_date_us_slash() {
        assert_eq!(parse_date_to_ms("1/15/2025"), Some(1_736_899_200_000));
        assert_eq!(parse_date_to_ms("01/15/2025"), Some(1_736_899_200_000));
    }

    #[test]
    fn parse_date_named_month_short() {
        assert_eq!(parse_date_to_ms("Jan 15 2025"), Some(1_736_899_200_000));
        assert_eq!(parse_date_to_ms("jan 15 2025"), Some(1_736_899_200_000));
    }

    #[test]
    fn parse_date_named_month_long() {
        assert_eq!(
            parse_date_to_ms("January 15, 2025"),
            Some(1_736_899_200_000)
        );
        assert_eq!(parse_date_to_ms("January 15 2025"), Some(1_736_899_200_000));
    }

    #[test]
    fn parse_date_all_months() {
        // Spot-check a few month names
        assert!(parse_date_to_ms("February 1 2025").is_some());
        assert!(parse_date_to_ms("Mar 31 2025").is_some());
        assert!(parse_date_to_ms("December 25 2025").is_some());
    }

    #[test]
    fn parse_date_leap_day() {
        // 2024 is a leap year; 2025 is not.
        assert!(parse_date_to_ms("2024-02-29").is_some());
        assert!(parse_date_to_ms("2025-02-29").is_none());
    }

    #[test]
    fn parse_date_invalid() {
        assert!(parse_date_to_ms("not-a-date").is_none());
        assert!(parse_date_to_ms("2025-13-01").is_none()); // month 13
        assert!(parse_date_to_ms("2025-01-32").is_none()); // day 32
        assert!(parse_date_to_ms("İan 1 2025").is_none());
        assert!(parse_date_to_ms("").is_none());
    }

    #[test]
    fn parse_jump_command() {
        assert!(matches!(
            parse("/jump 2025-01-15"),
            Command::JumpToDate(1_736_899_200_000)
        ));
        assert!(matches!(parse("/jump"), Command::Invalid(_)));
        assert!(matches!(parse("/jump garbage"), Command::Invalid(_)));
    }

    #[test]
    fn parse_top_command() {
        assert!(matches!(parse("/top"), Command::JumpToTop));
        // Stray trailing whitespace still resolves to the command.
        assert!(matches!(parse("/top   "), Command::JumpToTop));
    }
}
