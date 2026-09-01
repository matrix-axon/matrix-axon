# axon-tui

Terminal client for the Axon API.

## Run

Start Axon first, then run:

```bash
cargo run -p axon-tui -- --base-url http://127.0.0.1:8080
```

Options:

```bash
--base-url URL      Axon server URL, default http://127.0.0.1:8080
--account-id UUID   Restrict account and room views to one Axon account
--token TOKEN       Axon bearer token
```

The bearer token is read from `--token`, then `AXON_TOKEN`, then
`[server].bearer_token` in the config file. Empty token values are ignored.
The server URL is read from `--base-url`, then `AXON_BASE_URL`, then
`[server].base_url`, then the loopback default.

## What works today

- Lists rooms returned by `GET /v1/rooms`.
- Shows the latest timeline page for the selected room.
- Appends live events from `/v1/ws` for the selected room.
- Tracks unread counts for live events in other rooms.
- Hides most state events by default, while still showing room membership changes such as joins and leaves.
- Sends messages to the selected room (`POST /v1/.../send`).
- Edits the selected message (`PUT /v1/.../events/{event_id}`).
- Redacts the selected message (`DELETE /v1/.../events/{event_id}`).
- Reacts to the selected message with an emoji (`POST /v1/.../events/{event_id}/reactions`).
- Uploads and sends a local file to the selected room with `/send <path> [caption]` (`POST /v1/.../media/uploads` then `POST /v1/.../send-media`), with filesystem Tab completion and drag-and-drop: dropping a file into the terminal window fills in its path.
- Withdraws the current user's reactions by redacting their reaction events.
- Replies to a message (`/reply`, `r`) and starts a thread from one (`/thread`, `t`), sending with `reply_to`/`thread_root` on `POST .../send` (ADR 0032). Renders a reply-context preview line above a replying event and a `↳ N replies` badge on thread roots, resolved from the loaded slice with cross-window fallback lookups.
- Logs Matrix accounts in through Axon's lifecycle API (Axon resolves the homeserver server-side), with masked password entry.
- Logs active accounts out while retaining their archived data.
- Multi-account panel and account filtering, with keyboard navigation and search across Accounts, Rooms, and Messages.
- Own messages appear in a distinct configurable color.
- Renders Matrix `formatted_body` HTML for timeline messages when present, with sanitized support for common inline and block formatting.
- Renders image and sticker thumbnails inline, with an explicit larger preview for the selected image.
- Syncs per-room message drafts across devices through Axon's device-state API (M12): a draft typed here appears on the user's other clients within about a second, survives restarts, and clearing or sending it clears it everywhere. Each install mints a device UUID on first run, stored in `device-id` next to the config file.
- Syncs per-room read markers across devices through the same API: reading a room here clears its unread badge on the user's other clients, and rooms with activity newer than their marker show as unread again after a restart (rooms never marked read are left alone). Markers only move forward — a stale marker from an offline device never resurrects a cleared badge.
- Tracks unread threads: thread roots show a bold `N new` count with a latest-reply preview, and the `/unreadthreads` picker (`Alt-T`) lists every thread with unseen replies across rooms and jumps straight into its panel. Opening the thread panel is what marks a thread read; unseen replies are found both live and, via the room read marker, on room entry after a restart.

## Not Yet Implemented

These are client-side gaps, not API gaps — the Axon API already supports them (e.g. `POST .../rooms/join`); the TUI just doesn't drive them yet. See `docs/client-parity.md` for the full cross-client picture.

- Joining a room from the TUI (leaving, forgetting, and moderation actions are covered below under Commands).
- Complete `/whereami` room details, including full alias and member lists.

## Commands

Type `/help` or `/?` in the entry line to show a popup with available commands. Type `/shortcuts` to see all keyboard shortcuts. To send a message beginning with `/` instead of running a command, type an extra leading slash; for example, `//help` sends `/help`. Slash-command responses that do not fit in the entry box open in a scrollable popup instead. Popups are dismissed with `Esc`.

| Command                                 | Behavior                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| --------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| plain text                              | Send a message to the current room.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `//<text>`                              | Send a message beginning with a literal `/`; for example, `//help` sends `/help`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `/login [user] [password] [homeserver]` | Log in a Matrix account. Usernames accept `@user:domain`, `user:domain`, or `user@domain`; typing the homeserver host (e.g. `@user:matrix.domain`) is rejected with a hint naming your canonical Matrix ID. The optional third argument overrides homeserver resolution — e.g. `/login @user:example.com pw matrix.example.com` (a bare host gains `https://`; pass an explicit scheme for loopback, e.g. `http://localhost:8008`). The inline password is a single token; for a password with spaces, omit it (and optionally give the homeserver after the Matrix ID, e.g. `/login @user:example.com hs.example.com`) to type it at the hidden prompt. Missing fields are prompted for and passwords are always masked. After a new or reactivated unverified login, the TUI offers a masked recovery-key prompt; submit an empty value or press `Esc` to skip it. |
| `/logout [user]`                        | Log out an active account while retaining its archive. Accepts `@user:domain`, `user:domain`, `user@domain`, or a unique localpart; Tab/Shift-Tab cycles matches. If legacy duplicate rows share one Matrix ID, completion selects them by account UUID. Prompts for `[y/N]` confirmation unless `display.confirm_logout = false`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `/recover [user]`                       | Import Secure Backup and cross-signing keys for an active account, then retry decryption of stored messages. The optional target uses the same Matrix-ID, localpart, UUID, and Tab/Shift-Tab selection rules as `/logout`; when only one active account matches it is selected automatically. The recovery key is accepted only at the masked prompt, never inline. Submit an empty value or press `Esc` to cancel.                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `/backup enable [user]`                 | Originate or kick megolm key backup for a **verified** active account (`POST …/backup/enable`). Same account targeting and Tab/Shift-Tab cycling as `/recover`. The recovery key is accepted only at the masked prompt, never inline. Empty Enter omits the key (kick-upload only); `Esc` cancels. Unverified accounts are refused before the key is sent. Success copy reports `backup_action` (`enabled` / `joined` / `already_uploading` / `export_pending` / `failed`).                                                                                                                                                                                                                                                                                                                                                                                          |
| `/delete [user]`                        | Permanently delete an account and its local Axon data. Accepts `@user:domain`, `user:domain`, `user@domain`, or a unique localpart; Tab/Shift-Tab cycles matches. If legacy duplicate rows share one Matrix ID, completion selects them by account UUID. Deletion requires typing `YES` in all caps at the confirmation prompt.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `/room <room>` (`/switch` alias)        | Switch visible rooms by list number, room id, canonical alias, display name, or shortened alias.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `/account <account>`                    | Filter rooms by Matrix ID, localpart, or the number shown in the account list. Account `0` (or `all`) shows all accounts.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `/status`                               | Show Axon connectivity, the current account filter, and all accounts as `logged in` or `logged out`, plus each account's megolm-backup snapshot (homeserver existence, this-device uploading, backup state, 4S secret-storage state). 4S `enabled` is not "history keys imported."                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `/event <event_id>`                     | Show a compact status-line summary of one event in the selected account.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `/whoami`                               | Show your Matrix ID, profile display name, and this Axon session's Matrix device id (and device display name when the homeserver has one) for the selected room's account.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `/whereami`                             | Show a room information popup for the selected room. Up/Down/PageUp/PageDown scroll the popup.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `/send <path> [caption]`                | Upload a local file and send it to the current room, with an optional caption. `<path>` Tab-completes against the filesystem; dragging a file into the terminal window also fills it in (quoted or backslash-escaped paths from the drop are unescaped automatically). Honors a pending `/reply` or open thread the same way a plain send does.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `/leave`, `/part`                       | Leave the selected room after `[y/N]` confirmation. The room list refreshes from Axon after the homeserver accepts the leave.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `/forget [room]`                        | Forget the selected room, or a room resolved like `/room <room>`, after `[y/N]` confirmation.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `/invite <user>`                        | Invite a Matrix user ID to the selected room.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `/kick <user> [reason]`                 | Kick a Matrix user ID from the selected room after `[y/N]` confirmation.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `/ban <user> [reason]`                  | Ban a Matrix user ID from the selected room after `[y/N]` confirmation.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `/unban <user> [reason]`                | Unban a Matrix user ID from the selected room after `[y/N]` confirmation.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `/react [emoji]`                        | React to the selected message, or the most recent displayed message when none is selected. With an emoji or shortcode such as `/react +1`, send immediately; without one, open the selector.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `/unreact`                              | Withdraw one of your reactions from the selected or most recent displayed message. A sole reaction is withdrawn immediately; Tab cycles when several exist.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `/reply`                                | Reply to the selected or most recent displayed message.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `/thread`                               | Start a thread from the selected or most recent displayed message.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `/unreadthreads`, `/ut`                 | Open the unread-thread picker: threads with unseen replies across all rooms, with sender/body previews. `Enter` jumps to the thread and opens its panel; `Esc` closes. Also on the `unread_threads` shortcut (default `Alt-T`).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `/shortcuts`                            | Show active keyboard shortcuts from the config file.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `/help`, `/?`                           | Show available slash commands.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `/refresh` (`/rooms` alias)             | Refresh the room list and redraw the terminal display.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `/quit`                                 | Exit.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `/join <room>`                          | Known command for joining a room; pending TUI-M19-2 implementation.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |

Room switching is forgiving. For a room with canonical alias
`#test:example.com`, all of these can match:

```text
/room 1
/room test
/room #test
/room test:example.com
/room #test:example.com
```

Use Tab to complete slash commands, `/room` room names, selected-room users for
`/invite`, `/kick`, `/ban`, and `/unban`, and emoji names after
`/react`; it also cycles active accounts for `/logout`, `/recover`, `/backup enable`, and `/account`, plus all
client-visible accounts for `/delete`, and cached selected-room member user IDs for
`/invite`, `/kick`, `/ban`, and `/unban` when those members are already known locally.
Use Shift-Tab to cycle backward through matching options. When several
visible rooms match `/room`, completion advances to their
longest common prefix and lists the remaining suffixes. Enter reports an
ambiguity until the text identifies one room. While Tab completion is partial,
Enter keeps the command open instead of submitting it. A unique Tab match is
replaced with that room's canonical alias or room ID.

`/send <path>`'s path argument also Tab-completes against the filesystem the
same way, advancing to the longest common prefix or cycling matches; a
completed directory keeps a trailing `/` so completion can continue into it.
Dropping a file into the terminal window fills in its path directly (real
bracketed-paste support, not just tolerated keystrokes), including a path the
terminal wraps in quotes or backslash-escapes.

## Keyboard Shortcuts

Defaults:

| Shortcut   | Behavior                                                                                                                     |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `Ctrl-Tab` | Cycle focus: Input → Accounts → Room List → Message List when multiple accounts are active.                                  |
| `Ctrl-N`   | Next room (always active).                                                                                                   |
| `Ctrl-P`   | Previous room (always active).                                                                                               |
| `Alt-N`    | Next account filter when multiple accounts are active.                                                                       |
| `Alt-P`    | Previous account filter when multiple accounts are active.                                                                   |
| `Alt-U`    | Toggle unread filter: show only rooms with unread messages. The Rooms pane heading changes to `Rooms (Unread)` while active. |
| `Alt-T`    | Open the unread-thread picker (same as `/unreadthreads`).                                                                    |
| `Ctrl-J`   | Next displayed message (always active).                                                                                      |
| `Ctrl-K`   | Previous displayed message (always active).                                                                                  |
| `Ctrl-C`   | Quit.                                                                                                                        |

When focus is on the **Room List** or **Message List**, the focused pane border is highlighted:

| Shortcut              | Behavior                                                              |
| --------------------- | --------------------------------------------------------------------- |
| `Up` / `Down`         | Navigate items in the list.                                           |
| `PageUp` / `PageDown` | Page through items.                                                   |
| `/`                   | Start a search; type a query and press `Enter`.                       |
| `n`                   | Next search match (no wrap).                                          |
| `N`                   | Previous search match (no wrap).                                      |
| `v`                   | In Message List, open a larger preview of the selected image message. |
| `Enter` or `Esc`      | Return focus to Input.                                                |

When the **Search Results** popup is open:

| Shortcut              | Behavior                                                      |
| --------------------- | ------------------------------------------------------------- |
| `Up` / `Down`         | Select the previous or next result.                           |
| `PageUp` / `PageDown` | Page through results.                                         |
| `Home` / `End`        | Jump to the first or last result.                             |
| `s`                   | Toggle newest-first or oldest-first ordering.                 |
| `g`                   | Toggle straight time ordering or room grouping.               |
| `Enter`               | Jump to the selected result in the room timeline.             |
| `r`                   | Reply to the selected result, after jumping to it.            |
| `t`                   | Start a thread from the selected result, after jumping to it. |

When focus is on the **Input** pane:

| Shortcut              | Behavior                                                                                    |
| --------------------- | ------------------------------------------------------------------------------------------- |
| `Enter`               | Submit the entry line (sends a message or runs a slash command).                            |
| `Esc`                 | Clear the entry line and abort any pending action or message selection.                     |
| `PageUp` / `PageDown` | Page through messages without changing focus.                                               |
| `e`                   | Edit the selected message (pre-fills the input; `Esc` to cancel).                           |
| `d`                   | Redact the selected message immediately.                                                    |
| `Shift-R`             | React to the selected message: type an emoji name, `Tab` to cycle matches, `Enter` to send. |
| `Shift-U`             | Withdraw one of your reactions from the selected message; `Tab` cycles when several exist.  |
| `r`                   | Reply to the selected message.                                                              |
| `t`                   | Start a thread from the selected message.                                                   |
| `Ctrl-A`, `Home`      | Move to start of the entry line.                                                            |
| `Ctrl-E`, `End`       | Move to end of the entry line.                                                              |
| `Left` / `Right`      | Move within the entry line.                                                                 |
| `Up` / `Down`         | Select the previous or next timeline message for editing.                                   |
| `Backspace`           | Delete before the cursor.                                                                   |
| `Delete`              | Delete after the cursor.                                                                    |
| `Ctrl-U`              | Kill line (erase all typed text).                                                           |
| `Tab`                 | Complete a slash command, room name, or emoji (during reaction entry).                      |

## Configuration

On first run, `axon-tui` creates a default config file at:

```text
$XDG_CONFIG_HOME/axon-tui/config.toml
```

If `XDG_CONFIG_HOME` is unset, it uses:

```text
~/.config/axon-tui/config.toml
```

The app repairs older config files by adding missing default keys on startup.

Example:

```toml
[shortcuts]
next_room = "ctrl-n"
previous_room = "ctrl-p"
quit = "ctrl-c"
complete = "tab"
submit = "enter"
clear_input = "esc"
backspace = "backspace"
cursor_start = "ctrl-a"
cursor_end = "ctrl-e"
cursor_left = "left"
cursor_right = "right"
edit_previous = "up"
edit_next = "down"
media_preview = "v"
message_down = "ctrl-j"
message_up = "ctrl-k"
message_page_up = "pageup"
message_page_down = "pagedown"
reply = "r"
thread = "t"
edit_message = "e"
redact_message = "d"
react_message = "shift-r"
unreact_message = "shift-u"
unread_threads = "alt-t"
focus_next = "ctrl-tab"

[colors]
border = "gray"
selected_room = "cyan"
unread_count = "yellow"
message_sender = "green"
own_message_sender = "light-cyan"
input_hint = "dark-gray"
status = "cyan"
background = "default"
# accounts_foreground = "default"
# rooms_foreground = "default"
# messages_foreground = "default"
# input_foreground = "default"
# accounts_background = "default"
# rooms_background = "default"
# messages_background = "default"
# input_background = "default"
# popup_background = "default"

[display]
debug = false
show_state_events = false
message_density = "normal"
input_lines = 1
confirm_logout = true
```

Supported key forms include `ctrl-n`, `ctrl-j`, `ctrl-k`, `ctrl-tab`, `tab`,
`enter`, `esc`, `backspace`, `home`, `end`, `up`, `down`, `left`, `right`,
`pageup`, `pagedown`, `space`, `r`, `t`, `shift-r`, `shift-u`.

Supported color names are `black`, `red`, `green`, `yellow`, `blue`, `magenta`,
`cyan`, `gray`, `dark-gray`, `light-red`, `light-green`, `light-yellow`,
`light-blue`, `light-magenta`, `light-cyan`, `white`, and `default` (terminal
default color).

Set `display.show_state_events = true` to show all state events in room
timelines. When it is `false`, membership events such as joins, leaves, bans,
and invites are still shown.

Set `display.message_density` to choose how messages are laid out. The default
is `"normal"`: the sender and timestamp sit on their own line and the message
begins on the next line, indented to align under the sender; the sender is the
display name when known, otherwise the full Matrix ID such as
`@alice:example.com`. Set it to `"dense"` to put the sender, timestamp, and
message start on one line (wrapped lines align under the body); the sender is
shortened to the display name or bare `@localpart` (no homeserver).

Set `display.input_lines` to control the height of the command/entry box.
The default is `1`; set it higher for composing multi-line messages.

Set `display.confirm_logout = false` to skip the `[y/N]` confirmation prompt
and log out immediately. The default is `true`.

Set `display.debug = true` to show Matrix event IDs in the command/entry box
status text. The default is `false`, which hides those event codes.

Set `colors.own_message_sender` to control the color used for the sender label
on messages you sent. Defaults to `"light-cyan"` to distinguish them from other
senders (controlled by `colors.message_sender`).

Set `colors.background` to apply a uniform background color to all panes.
Defaults to `"default"`, which leaves the terminal's own background unchanged.

For finer control, each pane has an independent foreground and background color
setting: `accounts_foreground`, `rooms_foreground`, `messages_foreground`, and
`input_foreground` control the base text color of uncolored content in each
pane; `accounts_background`, `rooms_background`, `messages_background`, and
`input_background` override the pane background. Per-pane background settings
fall back to `colors.background` when not specified; per-pane foreground
settings fall back to the terminal default. Named foreground colors (such as
`border` and `message_sender`) always take precedence over the pane foreground.

Set `colors.popup_background` to control the background color of all popups
(`/help`, `/shortcuts`, `/whereami`, `/status`). Defaults to
`colors.background` when not specified.

The default config file includes four ready-to-use commented-out themes
(Dracula, Nord, Solarized Dark, and Paper Light) that can be activated by
uncommenting the relevant lines and commenting out the Default theme block.

Set `display.search_wrap = false` to make search stop at the end/beginning of
the list instead of wrapping around. The default is `true`.

Pane widths (set interactively with `Alt-Left`/`Alt-Right` in the accounts or
rooms panel, or `Alt-Up`/`Alt-Down` for input height) are session-only by
default. Run `/saveconfig` to persist the current `input_lines`,
`accounts_panel_width`, and `rooms_panel_width_adj` to the config file. The
saved values appear in `[display]` and are restored on the next launch.

Run `/editconfig` to open the config file directly in your `$EDITOR`. If
`$EDITOR` is not set, `nano` is used on macOS/Linux and `notepad` on Windows.
The TUI suspends while the editor is open and reloads the config automatically
when the editor exits. Server URL and bearer-token changes take effect on the
next launch.

Whenever axon-tui rewrites the config, it preserves supported settings and
comments. Unsupported options are retained as commented-out lines with an
explanatory comment immediately above them instead of being deleted.

## Media

Image messages and stickers reserve a fixed six-row thumbnail card below their
caption. The TUI renders a thumbnail only when that complete card is visible,
so terminal graphics cannot overlap adjacent message text while scrolling.
With Message List focused, select an image and press `v` (configurable as
`shortcuts.media_preview`) to open a larger popup preview.

Media is requested from Axon's account-scoped `/v1/media` proxy only when it is
visible or explicitly previewed. Downloads, image decoding, EXIF orientation,
resizing, and terminal-protocol encoding run as bounded background work so room
navigation and input remain responsive. Decoded images are dimension-checked
and downscaled before caching. The client keeps at most 16 decoded images and
32 encoded terminal images, runs at most four media workers, and rejects media
responses larger than 20 MiB. Kitty and iTerm2 are selected from safe terminal
environment hints; half-block rendering is the portable fallback. Set
`AXON_IMAGE_PROTOCOL` to `kitty`, `sixel`, `iterm2`, or `halfblocks` to override
detection without running a terminal capability query.

### Sixel rendering

Sixel graphics are not ordinary text cells. Terminals and multiplexers can drop
the graphics layer even though axon-tui's text-cell buffer has not changed, so
ratatui may otherwise skip repainting an image it believes is still present.
Inside tmux, axon-tui retransmits visible Sixel thumbnails every five seconds,
and does the same for an open Sixel preview. This keeps images visible, but the
retransmission may cause a brief flicker.

Possible workarounds:

- Use halfblocks inside tmux for the most reliable, flicker-free rendering:
  `AXON_IMAGE_PROTOCOL=halfblocks axon-tui`.
- If tmux's status refresh is triggering the disappearance, disable its periodic
  update with `tmux set -g status-interval 0`. This affects the whole tmux
  server; set a nonzero interval later to restore automatic status updates.
- If automatic cell-size detection is unavailable or inaccurate, set the
  terminal's character-cell size explicitly, for example
  `AXON_FONT_SIZE=9x18 axon-tui`. The format is width-by-height in pixels.
- Set `AXON_NO_IMAGE_QUERY=1` to disable startup terminal queries. Unless an
  explicit protocol and usable font size are supplied, this falls back to
  halfblocks.

Windows Terminal supports the cell-size query used by axon-tui. In tmux, the
query and Sixel output require `allow-passthrough`; axon-tui enables it for the
current pane at startup.

## Formatted Messages

When an event includes Matrix HTML formatting (`content.format =
"org.matrix.custom.html"` and `content.formatted_body`), the TUI sanitizes it
and renders a small terminal-friendly subset: bold, italic, inline code, links,
block quotes, lists, paragraphs, line breaks, and preformatted code blocks.
Unsupported HTML is stripped, and the TUI falls back to plain `body` if the
formatted content produces no displayable text.
