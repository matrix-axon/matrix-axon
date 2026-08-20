# axon-tui — Contributor Notes

`axon-tui` is a terminal client for the Axon `/v1/` HTTP + WebSocket API. It is a client, not a Matrix SDK application: do not bypass Axon by talking directly to a homeserver or Matrix SDK from this crate.

## Scope

- Slash commands and keyboard shortcuts should reflect the Axon API surface. Unsupported Matrix actions should report that the current Axon API does not support them yet.
- The client reads accounts, rooms, and timeline history over HTTP, live events over `/v1/ws`, and sends lifecycle and message mutations over HTTP.
- Preserve the future path for reply threading, search, and scrolling back, but do not add server-side assumptions before endpoints exist.

## Terminal UX

- Treat the entry line like a small readline-style prompt: visible cursor, editable text, Tab completion, familiar terminal shortcuts (`Ctrl-A`, `Ctrl-E`, `Ctrl-U`), and `Up`/`Down` navigation through timeline messages for quick editing.
- Treat every multi-step TUI process as one uninterrupted interaction. From the first prompt until completion or cancellation, unrelated asynchronous updates must not replace its entry-line instructions, progress, validation errors, confirmation text, or user input. Background WebSocket, refresh, and timeline statuses may update their underlying state, but must defer visible entry-line status changes until the process ends; only outcomes belonging to the active process may advance its message.
- The layout uses an explicit `Mode` state machine for Accounts (when multiple accounts are active), Room List, Message List, and Input. Ctrl-Tab cycles focus; the focused pane border is highlighted. In list modes, arrow keys navigate items, `/` enters a search sub-mode, and `n`/`N` move to adjacent matches after the search is committed. Editing, reacting, unreacting, search, and popup interactions each have explicit modes.
- Keep keyboard shortcuts configurable through the config layer. When adding a new shortcut: add a default key to `RawConfig::default_values()` and all related structs (`RawShortcuts`, `PartialRawShortcuts`, `Shortcuts`), wire it through `into_shortcuts()`, `merge()`, and `to_toml()`, add it to the `DEFAULT_CONFIG` constant, include it in `popup_shortcuts_lines()`, and write a test.
- Keep slash commands discoverable through `/help`, `/?`, and Tab completion, including commands the TUI knows about but the current Axon API does not support yet. A text entry beginning with `//` sends a message beginning with a literal `/` instead of running a command, and this escape must remain documented in `/help`. `/help` and `/shortcuts` open popup overlays dismissed with `Esc`. `/shortcuts` does not need to document system-universal keyboard shortcuts like `enter` to send or `left-arrow` to move left.
- Keep short slash-command responses in the entry box. When a completed response would exceed the configured entry-box height at the current terminal width, show the full response in a scrollable popup dismissed with `Esc`.
- Keep argument completion consistent with command resolution. `/room` (legacy alias `/switch`) accepts visible-list numbers, room IDs, aliases, display names, and unique prefixes; matching and completion must honor the active account filter. Ambiguous Tab completion advances only to the longest common prefix and blocks Enter until the target identifies one room. `/account` filters active accounts using the panel's displayed numbers (`0` means all accounts), user IDs, or localparts. `/react` completes known emoji names and never sends arbitrary reaction text. `/logout` and `/recover` resolve only active accounts and cycle ambiguous targets with Tab/Shift-Tab; if duplicate rows share one Matrix ID, completion uses the account UUID so either row remains selectable. `/recover` accepts the recovery key only through its masked prompt, never inline. `/delete` uses the same Matrix-ID/localpart targeting and Tab/Shift-Tab cycling, including UUID disambiguation for duplicate rows, but allows both active and deactivated accounts and requires typing `YES` in all caps at the confirmation prompt. `/send <path> [caption]` Tab-completes `<path>` against the filesystem the same way (advance to longest common prefix, cycle full matches); the path token is quote/backslash-escape-aware (`command::parse_leading_path_token`, shared by parsing and completion) so a filename containing a space round-trips correctly.
- Bracketed paste (`crossterm::EnableBracketedPaste`, pushed alongside raw mode in `TerminalGuard`) delivers a terminal drag-and-drop or clipboard paste as one atomic `Event::Paste(String)` rather than a burst of key events; `App::handle_paste` bulk-inserts it via `insert_str` in the modes that accept free text (the same set `insert_char`'s callers gate on), so dropping a file onto the terminal window fills in its path directly. Never unquote a paste generically — only `/send`'s path-token parsing strips quotes/backslash-escapes, since a plain pasted message may contain intentional quote characters.
- Login credentials are transient. Normalize `user:domain` and `user@domain` to canonical `@user:domain`, validate with `ruma`, mask prompted and inline passwords, and clear password state on submission, failure, cancellation, or focus changes. Use the same normalization for logout targeting. Never send the password anywhere except Axon's login endpoint, and never talk to a homeserver directly: homeserver discovery is Axon's job (ADR 0023) — login forwards the Matrix ID and password, and `homeserver_url` only when the user supplies the optional `/login` third argument to override resolution (a bare host is given `https://`; an explicit scheme is preserved for loopback). A space-bearing password can't be given inline (the inline password is a single token); such users reach the hidden prompt via `/login` or `/login <user> [homeserver]`, where the username step also accepts an optional homeserver. Axon rejects an MXID written with the homeserver's hostname (`@user:matrix.domain`) with a 400 whose message suggests the canonical Matrix ID; the TUI shows API error messages verbatim, so no client-side handling is needed.
- `/whereami` shows the current room summary from Axon and any members learned from loaded timeline membership events. Do not present that derived member list as complete until Axon exposes a room-info or room-state API with full aliases, members, power levels, encryption, and access settings.
- `/status` uses the cached `GET /v1/accounts` response and lists every client-visible account as `logged in` (`active`) or `logged out` (`deactivated`). Keep the account panel and `/account` navigation active-only.
- Room switching should remain forgiving within the active account filter: visible-list number, room id, canonical alias, display name, and shortened Matrix alias forms should continue to work.

## Event loop and async

- **Reach the first frame before any network await (ADR 0093).** Startup runs
  as spawned stages applied through the main loop's channel, so the client
  paints and accepts keys immediately; a blank terminal during a slow load
  reads as a hang, not a load. Any new startup work joins that chain rather
  than adding a sixth await ahead of the loop. The one deliberate exception is
  the launch room's timeline, which is bounded and must follow read-marker
  hydration (ADR 0048/0089).
- **Per-room background work is demand-driven and semaphore-bounded (ADR
  0093).** Anything that costs one request per room — member reads for list
  titles, and any future per-room fetch — must ask only for what is on screen
  plus a small lookahead, hold a permit from a bounded pool, and record a
  negative answer so it stops asking. A per-room cooldown bounds repeats of one
  room; it does not bound how many rooms are in flight, which is the property
  that matters on a server with thousands of them.
- **Never `await` an API call from key handling or a draw-adjacent path.** Spawn the work as a task and apply its result through the main-loop outcome channel (the lifecycle verbs under _Mutations_ are the model). Blocking the loop freezes input and redraw.
- **Async results must be cancelable or stale-checked before they mutate mode or state.** A result that lands after the user has changed mode, room, or account must be dropped or reconciled against the current state, never applied blindly — the same discipline media workers use for evicted entries.
- **Historical navigation reuses the existing timeline window/cursor semantics.** A jump (search hit, thread jump, unread marker) must land in the same window/cursor machinery so `Home`/`End`/`PageUp`/`PageDown` keep working after the jump; do not add a parallel scroll path.
- **Reuse shared helpers.** Dates, timeline windows, snippets, and shortcut handling each have one helper — reuse it rather than reimplementing, so formats and edge cases stay consistent (the root AGENTS.md _No duplicate code_ convention, applied in the TUI).
- **Treat blank field values as errors; require an explicit wildcard for broad scope.** An empty search/filter/target field is a user error that gets useful feedback, not an implicit "match everything" — broad scope must be asked for explicitly.
- **Reading a room settles two positions, in two different orders (ADR 0089).** The cross-device read marker (`read_markers` device state, ADR 0048) is a _display-order_ artifact — where to draw the "new messages" line — forward-only on `origin_ts`, and all unread detection compares against it. The Matrix read receipt is interpreted by the homeserver in _arrival_ order, so it names the greatest `EventDto::arrival_order` among the events actually displayed, tracked separately in `App::receipt_targets`. **"Displayed" is the pair `should_show_event` _and_ `thread_visible`, matching `selected_events` — never either alone.** `should_show_event` never looks at `thread_relation()`, so it passes a thread reply the main timeline hides behind its root's badge; `thread_visible` is the other half. Review on #165 caught the receipt pick using only the first, which could name an unpromoted backfilled reply — `collect_unseen_thread_promotions` promotes nothing on a room's first load (no marker) and never promotes a backfilled event anyway, since it requires `origin_ts > marker_ts`. **These are two values; do not merge them, and do not make one a passenger on the other.** They disagree whenever a bridge backfills a conversation into a freshly created portal: the room's only message is then oldest by `origin_ts` and newest by arrival order, so the marker is already ahead of it and can never move again, while the receipt still must. That is why `note_room_read` advances each independently and arms its debounced PUT if _either_ moved — gating the receipt on the marker would mean no already-broken room ever repairs itself. `receipt_targets` is session-local and deliberately never hydrated, which is what makes that repair happen on the first open after a restart.
- **Per-room state carries the room key; it never lives in one "current room" slot.** Compose buffers, drafts, and pending send queues each have one physical slot but N logical owners (rooms): key them by room (`compose_room`, a `RoomKey`-keyed pending queue) or flush on room switch. This is the TUI instance of the root AGENTS.md "Design guardrails" rule 2 — see it for the general rule and the concurrent-work-loss failure it prevents (PR 192 draft-sync feedback).

## Media

- Fetch media only through Axon's account-scoped `/v1/media` proxy. Cache keys must include `account_id`; an `mxc://` URL alone is not an Axon resource identity.
- Keep media work demand-driven and bounded. Request only visible thumbnails or an explicitly opened preview, cap response size, bound decoded-image and encoded-protocol caches, and limit concurrent workers.
- Never download, decode, apply EXIF orientation, resize, or encode a terminal image on the input or draw loop. Those operations belong in background work, and late results for evicted entries must be discarded.
- Do not probe terminal image capabilities by reading stdin before launch; unsupported terminals can leave a detached reader that steals keystrokes. Use safe environment hints, an explicit `AXON_IMAGE_PROTOCOL` override, and halfblocks as the fallback.
- Inline images own fixed rows in the message flow. Render a terminal graphic only when its complete reserved region is visible so scrolling cannot place it over neighboring text.
- Keep the larger image view explicit and modal rather than automatically changing the message-pane layout when selection moves.

## Rendering robustness

The terminal is a boundary too — terminal size, content width, and remote media are all adversarial.

- **Budget the whole area before allocating to one element.** Reserve space for captions, borders, status, and prompts first; never let a single element (a tall image preview, a long line) claim the full width/height and squeeze a sibling to zero.
- **Survive degenerate sizes.** Handle a 1-row / 1-column terminal and a very large one; use saturating arithmetic so layout math like `width - n` never underflows.
- **Measure display width, not bytes or `char` count** — grapheme / East-Asian width drives wrapping and truncation (see `wrap.rs`).
- **Every fetch the TUI makes to axon has a timeout, media included.** `api.rs` already buckets HTTP timeouts; any new path (image/media fetch, a worker pool) must adopt the same — a hung fetch must not block input or permanently consume a bounded fetch pool.
- **Wrap all message-pane continuation lines through `wrap_rich_lines` before rendering.** Thread badges, reply context, and reaction lines must be wrapped to `continuation_body_width(width)` before being pushed as `Line` values. A raw `Line::from(long_text)` that exceeds the inner panel width overwrites the right border `│` with text, which then persists every frame because the Paragraph re-renders before the border is re-drawn.
- **Subtract all first-line label injections from `first_body_width` before wrapping body text.** Any label inserted into the header line after body text is wrapped (e.g. the `[thread root] ` label — trailing space included, 14 columns) causes the body to overflow the right border. Always compute the full first-line overhead — sender label + timestamp + any injected labels — and subtract the total from the width before calling body-wrapping functions.
- **Use `width_cjk()` not `width()` when measuring terminal column widths.** Characters with Unicode "East Asian Ambiguous" width (e.g. `·` U+00B7 MIDDLE DOT, `■` U+25A0 BLACK SQUARE) are rendered as 2 columns by most terminals but return 1 from `unicode_width::UnicodeWidthChar::width()`. All wrapping and truncation logic in `wrap.rs` and similar helpers must use `char::width_cjk()` so that lines stay within their column budget in CJK-mode terminals.
- **Do not apply `CellDiffOption::AlwaysUpdate` to cells that image protocol widgets have already marked, and do not apply it to cells within expected image thumbnail rects.** Sixel, Kitty, and iTerm2 widgets set `CellDiffOption::Skip` on cells they occupy; overwriting `Skip` with `AlwaysUpdate` causes ratatui's diff to emit a space escape sequence directly over the just-rendered image, destroying it. When force-clearing a region (e.g. via `force_terminal_clear`), only apply `AlwaysUpdate` to cells whose `diff_option` is still `CellDiffOption::None` — the per-frame buffer-reset default for cells that hold no image data. Additionally, skip cells inside `thumb_specs` image rects even when the image is in `Encoding` state (not yet rendered): blank cells in those rects look identical to ghost-pixel cells but hold the old sixel data; clearing them before the new-width encoding completes causes a visible flash.
- **Any code that inserts new lines into existing messages inflates `total_lines` and must not force the viewport forward.** Thread badges, reply contexts, reactions, and any future per-message metadata are added after a background fetch; when `messages.scroll == usize::MAX` (follow-tail mode), this inflates `max_scroll` and scrolls images that were near the top of the viewport above the scroll boundary. Set `messages.scroll_follow_tail = false` in the metadata handler and restore it to `true` only when a real live tail event or explicit user navigation fires. Never leave `scroll_follow_tail` in an inconsistent state relative to `messages.scroll`.

## Mutations

Account lifecycle operations map to:

- Login: `POST /v1/accounts/login`
- Logout: `POST /v1/accounts/{account_id}/logout`
- Recover: `POST /v1/accounts/{account_id}/recover`
- Delete: `DELETE /v1/accounts/{account_id}`

Login, logout, and recover return `{ "data": <AccountDto> }`. Recover requires an active account and consumes a transient `recovery_key` without persisting it. Logout is non-destructive: the returned account is `deactivated` and its archive remains available for a later login. Delete returns `204 No Content` and permanently removes the account and its local Axon data.

Login, logout, recover, and delete run off the event loop: secret-bearing calls are spawned as tasks that own and then drop the password or recovery key, and results land back through a channel the main loop drains, so the UI keeps redrawing and a second lifecycle verb is refused while one is in flight. Login is idempotent server-side — an already-`active` account is returned unchanged with the password never consulted — so the client reports that no-op distinctly (`already logged in: … (no changes)`) by comparing the returned `account_id` against the accounts that were active before the attempt. After a real new/reactivated login, prompt for a recovery key only when the returned account is not verified; empty Enter or `Esc` skips. Logout asks for `[y/N]` confirmation unless `display.confirm_logout = false`. Delete always asks for an explicit `YES` confirmation.

Login, logout, recover, and delete follow the general multi-step interaction rule above: while a lifecycle prompt is open or a request is in flight, background statuses must not overwrite the entry line. Lifecycle outcomes may replace their own status when the operation advances or completes.

The four write operations map to these Axon API endpoints:

- Send: `POST /v1/accounts/{account_id}/rooms/{room_id}/send`
- Edit: `PUT /v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}`
- Redact: `DELETE /v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}`
- React: `POST /v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}/reactions`

All return `{ "data": { "event_id": "..." } }`.

`Mode::Editing`, `Mode::Reacting`, and `Mode::Unreacting` own their respective input flows. Redact fires immediately with no pending state. Unreact uses the redaction endpoint against the current user's reaction event ID; when several distinct reactions exist, Tab cycles the choices before Enter confirms.

Send media (ADR 0059/0062, `/send <path> [caption]`) is a two-step client call, off the event loop like the other mutations above:

- Stage: `POST /v1/accounts/{account_id}/media/uploads?kind=<image|file>&filename=<name>` — raw bytes as the request body, returns `{ "data": <StagedUploadDto> }` with `upload_id`.
- Send: `POST /v1/accounts/{account_id}/rooms/{room_id}/send-media` — JSON `{ "upload_id", "caption"?, "reply_to"?, "thread_root"? }`, returns `{ "data": { "event_id": "..." } }`.

`kind`/`Content-Type` are inferred client-side from the file extension (`media_kind_and_content_type` in `app.rs`), good enough to satisfy the server's `kind=image` ⇒ `image/*` validation. `/send` reuses `pending_reply`/`pending_thread` exactly like a plain send, and a `media_send_busy` flag (mirroring `lifecycle_busy`) refuses a second `/send` while one is uploading so its status line survives. No optimistic local echo — the sent event arrives over `/v1/ws`.

## Own-message identification

The TUI can seed the user's own Matrix ID from `RoomDto.account_user_id` when the server provides it, so own-message coloring works on first render. Keep that field optional in the client DTO for compatibility with older Axon servers. As a fallback, after `send_message_to_room` succeeds, the TUI stores the returned event_id as `pending_own_event_id`; when the echo arrives via the live WebSocket, it records `account_id → sender` in `own_senders`. Messages from that sender/account pair render with `colors.own_message_sender` instead of `colors.message_sender`.

## Formatted Messages

Matrix `formatted_body` is HTML, not Markdown. Render it only when `content.format == "org.matrix.custom.html"`, sanitize it before display, and keep support deliberately small and terminal-friendly. The current renderer handles common tags such as bold, italic, inline code, links, block quotes, lists, paragraphs, line breaks, and preformatted code blocks, then falls back to plain `body` when formatted content is absent or empty after sanitization. Do not render arbitrary HTML or add browser-like layout behavior.

## Config

- The config file lives at `$XDG_CONFIG_HOME/axon-tui/config.toml`, falling back to `~/.config/axon-tui/config.toml`.
- On first run, create a default config file with all default shortcuts, colors, and display options.
- Existing config files must be backward-compatible. If new default keys are added, load older configs by filling missing defaults and rewrite the repaired file instead of failing startup.
- Every config rewrite must preserve supported settings and user comments. If an unsupported option would otherwise be removed, retain it as a commented-out line with an explanatory comment immediately above it.
- Invalid user-provided key names or color names may remain errors; missing fields should not.

## Verification

For TUI-only changes, run:

```bash
cargo fmt --all --check
cargo test -p axon-tui
cargo clippy -p axon-tui --all-targets --all-features -- -D warnings
```

Broaden to workspace checks when changing workspace dependencies, shared crates, or Axon API contracts.

Also update [`docs/demo-coverage.md`](../../docs/demo-coverage.md) **in the same
PR** when the change touches anything a demo scene renders. The demo videos are
regenerated by hand and can therefore go stale, and that table is the mitigation
ADR 0086 names for it: a capability that never reaches a demo is invisible
twice — absent from the videos anyone evaluating the project watches, and
unexercised by the driver that would notice it breaking. A cell names the scene
that covers it, so the claim is checkable: `axon-demo-tui --scene <name>`.
"Not covered" with a reason is worth more than a scene name that does not really
exercise the thing.
