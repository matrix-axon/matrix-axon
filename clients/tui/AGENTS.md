# axon-tui — Contributor Notes

`axon-tui` is a terminal client for the Axon `/v1/` HTTP + WebSocket API. It is a client, not a Matrix SDK application: do not bypass Axon by talking directly to a homeserver or Matrix SDK from this crate.

## Scope

- Slash commands and keyboard shortcuts should reflect the Axon API surface. Unsupported Matrix actions should report that the current Axon API does not support them yet.
- The client reads accounts, rooms, and timeline history over HTTP, live events over `/v1/ws`, and sends lifecycle and message mutations over HTTP.
- Preserve the future path for reply threading, search, and scrolling back, but do not add server-side assumptions before endpoints exist.

## Terminal UX

- Treat the entry line like a small readline-style prompt: visible cursor, editable text, Tab completion, familiar terminal shortcuts (`Ctrl-A`, `Ctrl-E`, `Ctrl-U`), and `Up`/`Down` navigation through timeline messages for quick editing.
- Treat every multi-step TUI process as one uninterrupted interaction. From the first prompt until completion or cancellation, unrelated asynchronous updates must not replace its entry-line instructions, progress, validation errors, confirmation text, or user input. Background WebSocket, refresh, and timeline statuses may update their underlying state, but must defer visible entry-line status changes until the process ends; only outcomes belonging to the active process may advance its message.
- The layout uses an explicit `Mode` state machine for Accounts (when multiple accounts are active), Room List, Message List, and Input. Ctrl-Tab cycles focus; the focused pane border is highlighted. In list modes, arrow keys navigate items, the `find` shortcut (Ctrl-F by default) enters a search sub-mode, and `n`/`N` move to adjacent matches after the search is committed. Editing, reacting, unreacting, search, and popup interactions each have explicit modes.
- Keep keyboard shortcuts configurable through the config layer. When adding a new shortcut: add a default key to `RawConfig::default_values()` and all related structs (`RawShortcuts`, `PartialRawShortcuts`, `Shortcuts`), wire it through `into_shortcuts()`, `merge()`, and `to_toml()`, add it to the `DEFAULT_CONFIG` constant, include it in `popup_shortcuts_lines()`, and write a test.
- Keep slash commands discoverable through `/help`, `/?`, and Tab completion, including commands the TUI knows about but the current Axon API does not support yet. A text entry beginning with `//` sends a message beginning with a literal `/` instead of running a command, and this escape must remain documented in `/help`. `/help` and `/shortcuts` open popup overlays dismissed with `Esc`. `/shortcuts` does not need to document system-universal keyboard shortcuts like `enter` to send or `left-arrow` to move left.
- Keep short slash-command responses in the entry box. When a completed response would exceed the configured entry-box height at the current terminal width, show the full response in a scrollable popup dismissed with `Esc`.
- Keep argument completion consistent with command resolution. `/room` (legacy alias `/switch`) accepts visible-list numbers, room IDs, aliases, display names, and unique prefixes; matching and completion must honor the active account filter. Ambiguous Tab completion advances only to the longest common prefix and blocks Enter until the target identifies one room. `/account` filters active accounts using the panel's displayed numbers (`0` means all accounts), user IDs, or localparts. `/react` completes known emoji names and never sends arbitrary reaction text. `/logout`, `/recover`, and `/backup enable` resolve only active accounts and cycle ambiguous targets with Tab/Shift-Tab; if duplicate rows share one Matrix ID, completion uses the account UUID so either row remains selectable. `/recover` and `/backup enable` accept the recovery key only through the masked prompt, never inline. `/backup enable` empty Enter kicks an already-enabled upload; Esc cancels. `/delete` uses the same Matrix-ID/localpart targeting and Tab/Shift-Tab cycling, including UUID disambiguation for duplicate rows, but allows both active and deactivated accounts and requires typing `YES` in all caps at the confirmation prompt. `/send <path> [caption]` Tab-completes `<path>` against the filesystem the same way (advance to longest common prefix, cycle full matches); the path token is quote/backslash-escape-aware (`command::parse_leading_path_token`, shared by parsing and completion) so a filename containing a space round-trips correctly.
- `/verify` with no argument opens the device picker (`GET …/devices` for the resolved account, omitting this session's device).
  The list stays hidden until this session's device id is known.
  `r` in that state re-reads the account list, and lists devices only once the id is known.
  Tab on `/verify` still completes other users in the selected room and does not complete device ids.
  `/verify <device_id>` and `/verify @user:server` start a flow directly.
  A verification that is already in progress is left as it is.
  Enter on a row verifies the account the list was opened for, even if the account filter changes while the picker is open.
  `r` and `p` are the exact lowercase letters.
  Caps Lock and Num Lock do not block them.
  Shift, Ctrl, and Alt do.
  `r` does nothing while a load is still inside its HTTP timeout, and starts another load after that.
  Refreshing a list that is already showing keeps those rows until the new list arrives.
  A failed refresh leaves that list in place.
  Device names drop control characters and bidi overrides before they are drawn.
- Bracketed paste (`crossterm::EnableBracketedPaste`, pushed alongside raw mode in `TerminalGuard`) delivers a terminal drag-and-drop or clipboard paste as one atomic `Event::Paste(String)` rather than a burst of key events; `App::handle_paste` bulk-inserts it via `insert_str` in the modes that accept free text (the same set `insert_char`'s callers gate on), so dropping a file onto the terminal window fills in its path directly. Never unquote a paste generically — only `/send`'s path-token parsing strips quotes/backslash-escapes, since a plain pasted message may contain intentional quote characters.
- Login credentials are transient. Normalize `user:domain` and `user@domain` to canonical `@user:domain`, validate with `ruma`, mask prompted and inline passwords, and clear password state on submission, failure, cancellation, or focus changes. Use the same normalization for logout targeting. Never send the password anywhere except Axon's login endpoint, and never talk to a homeserver directly: homeserver discovery is Axon's job (ADR 0023) — login forwards the Matrix ID and password, and `homeserver_url` only when the user supplies the optional `/login` third argument to override resolution (a bare host is given `https://`; an explicit scheme is preserved for loopback). A space-bearing password can't be given inline (the inline password is a single token); such users reach the hidden prompt via `/login` or `/login <user> [homeserver]`, where the username step also accepts an optional homeserver. Axon rejects an MXID written with the homeserver's hostname (`@user:matrix.domain`) with a 400 whose message suggests the canonical Matrix ID; the TUI shows API error messages verbatim, so no client-side handling is needed.
- `/whereami` paints the room summary immediately, then fills in Axon's cached `/metadata`, `/homeserver_details`, `/info` member counts, `/upgrade`, and `/members` reads off the event loop (ADR 0114, `app/room_info.rs`).
  Keep the wording honest: an unknown snapshot without `absence_confirmed_at` is never rendered as unset or unencrypted, null counts are never zero, and power levels are labeled as configured values (not permissions).
  A confirmed absence overrides a stale canonical-alias summary; redacted or invalid state never acquires an absence default.
  Keep "Advertised aliases" separate from "Aliases on your homeserver" and "Directory publication on your homeserver".
  Restricted join-rule membership conditions use the referenced room's canonical alias from the same account's room list when nonblank, falling back to its room ID.
  Resolve against all known rooms for that account, including ones hidden by the current filter; do not use another account's alias or issue extra discovery reads.
  Homeserver details retain stale values beside refresh progress, failures, and the next retry time; a conclusive denial removes the old value.
  The member list comes from the loaded `/members` projection, grouped by membership, with each person's configured power level rather than a resolved role.
  Say when the joined count exceeds that list, and never present the list as complete (#659).
  Cap only that popup read. Timeline display-name seeding and space timeline reads of `/members` stay uncapped, so a large roster can still resolve sender names.
  Cache the rendered member lines when a read settles. A frame only wraps those lines to the terminal width.
  `shortcuts.find` (Ctrl-F by default) filters that loaded list inside the popup.
  The match is a case-insensitive substring of the display name or Matrix ID, with Latin accents folded to their base letter, and the section reports how many loaded members match.
  Say that the room has more members than are loaded only when the joined count is larger than the loaded list.
  Typing brings the member heading into view. Configured page up and page down still scroll while the prompt is open.
  Esc clears an active filter before it closes the popup.
  Opening `/whereami` again starts from the full loaded list, including when the previous close was not Esc.
  The prompt ends when the popup closes or the selected room changes, so a later paste reaches the composer.
  Alt and Ctrl-Alt characters are typed into the prompt. Plain Ctrl stays a shortcut.
  A later refresh of the same room must keep the filter text and must not jump the scroll on its own.
  While the popup is open, `room_details.changed` for its account and room re-reads only the named source: `metadata`, `member_counts` (`/info`), or `homeserver_details`.
  Ignore unknown sources.
  Reopen, reconnect, and explicit refresh re-read all five sources; successful reads do not poll.
  Each source has at most one request in flight and one pending invalidation.
  A slow member list must not block metadata.
  Failed HTTP reads retry after 30, 60, 120, then 240 seconds while visible; notifications respect that source's backoff and a 404 does not schedule retries.
  Each source allows one reconnect to bypass its backoff until the scheduled retry; explicit refresh and reopening retry immediately.
  Membership changes re-read only `/members` after a two-second pause, no later than ten seconds into a burst.
  Tombstones and creation events re-read `/upgrade`; redactions re-read `/upgrade` and debounce `/members`.
  Metadata timeline events do not duplicate the persisted metadata notification.
  A refresh leaves each source's last value and error in place until its own new result arrives.
  Closing the popup cancels every source and its retry intents.
  Closing preserves each source's failure history; reopening retries immediately but a repeated failure keeps escalating its backoff.
  Changing the cached room resets that history.
  If the selected room changes under the open popup, the next tick fetches that room and resets the scroll.
  Paste is accepted only while that prompt is open inside the room-info popup.
- `/status` uses the cached `GET /v1/accounts` response and lists every client-visible account as `logged in` (`active`) or `logged out` (`deactivated`). Each account block also shows the live megolm-backup snapshot (`exists_on_server`, this-device uploading, `backup_state`, `recovery_state`). `recovery_state` is 4S completeness, not "history keys imported." Keep the account panel and `/account` navigation active-only.
- Room titles follow ADR 0118: nonblank `name`, then `canonical_alias`, then the server's `display_name`, then room ID.
  Never derive room titles from `/members`; those reads still hydrate timeline sender names.
  Apply `room_display_name.changed` and title state events by account and room, retaining newer live fields across older room-list responses.
  Unknown-room frames are held with bounded storage rather than fetching the room list per frame.
  Overflow recovery coalesces reads until a snapshot requested after the latest dropped frame can cover it; failed reads allow the next overflow frame to retry.
  Reconnect and `/refresh` recover other missed updates.
- Room switching should remain forgiving within the active account filter: visible-list number, room id, canonical alias, display name, and shortened Matrix alias forms should continue to work.
- `/join <room id, alias, or matrix.to / matrix: link>` joins that room for the account filter, or for the only active account.
  A `504` does not retry and does not decide the outcome; the room list is what shows a join that landed.
  When the list right after the join does not contain the room, two further reads a few seconds apart open it once a list does.
  Choosing another room or starting another room action cancels that watch.
  An action that is held back, or one that is already in progress, does not.
  The watch also ends if no covering list arrives before its cap, so a later refresh can use the entry line.
  A join that finishes during a prompt does not change the selected room or the entry line until that prompt ends.
  A failed join, or a `504` the list does not show, puts `/join` back when the composer is still empty.
  That restored argument is what the user typed, so a link keeps its `via` hints.
  A `504` whose room list cannot be read says the result is unknown and still watches.
  A timeline read that fails stays on the entry line next to the joined room.

## Pending invites (ADR 0130)

- Selectable room-list rows stay joined rooms.
  Do not add an invite row to that list or to its keyboard navigation.
- When the filtered count is above zero, the rooms title appends ` · N invites` after the title it already has, including a bootstrap or search title and the unsaved-order suffix.
  Zero invites leaves that title unchanged.
  One invite uses the singular.
- `/pending` opens the inbox popup.
  Enter accepts, the `reject_invite` shortcut rejects, and Esc closes.
  `reject_invite` defaults to `r` with no modifiers, so Shift-R and Ctrl-R do not reject unless the user binds them.
  A submit binding of that same key accepts, and the popup title then omits reject.
  The title also omits reject when that key is a shortcut handled before the popup.
  A page binding is checked after reject, so the title still names it.
  The popup title otherwise names the configured accept, reject, and close keys.
  Reject asks `Reject {title}? [y/N]` and stays in the inbox.
  The inbox popup stays up under that question, and each frame puts the question back on the status line.
  `y` sends the leave for that invite.
  If that invite has already left the list, `y` does not send and says it is no longer pending.
  `n` and Esc cancel and leave the composer as it was.
  A reject that is still held, waiting on its first check, shows the pending status and does not ask.
  The question does not mark a room action busy and does not clear a join watch.
  Focus-cycle leaves the question the same way it leaves a room-action confirmation, and it does not reject.
  Up and Down move the selection by the invite's account and room.
  Page Up and Page Down, and the configured message page keys, move by eight.
  A filter that hides the selected row moves the marker onto an invite that is still visible, and that same keystroke does not accept or reject it.
- The list is always `GET /v1/invites` with no `account_id`.
  The visible set then uses the room list's account filter and room filter.
  Favorites show none.
  Unread keeps every invite that passed the account filter.
- A 2xx accept or reject suppresses that key until a later list omits it, so an in-flight list cannot put the row back.
  Both also request that list.
  The accept does not clear suppression itself.
  `invite.added` clears the suppression.
  `invite.removed` drops the row and does not suppress it.
- A 404 on the list is an older server: an empty inbox and no error.
  It does not clear suppression and does not settle a pending reject.
- A reject `504` keeps the row and uses the leave schedule (30/90/210/450 seconds, then the same grace).
  That hold is separate from pending membership and settles from `GET /v1/invites`.
  The schedule advance is shared with a pending leave.
  A repeat reject waits until the first check has run.
  A logged-out account is not settled by a list that omits it, and its hold does not advance or request a list until the account is active again.
- Accept uses join and then the existing room-open path.
  A 2xx closes the popup, requests a fresh invite list, and opens the room when the refreshed room list contains it.
  A `504` closes the popup, watches the room list when the room is not listed yet, re-reads the invite list immediately and twice more on those same delays, and does not restore `/join` into the composer.
  `/join` does not issue those invite reads.
  The timed-out room-list follow-up is one helper for both `/join` and an inbox accept.
- A known-contact DM invite may still flash and then disappear (ADR 0040).
- The startup invite read is spawned beside the room fetch.
  Applying it does not advance bootstrap.
  Login, logout, recover, backup enable, and delete request the list again after they refresh accounts.
  A target-resolution read that has not changed the active set yet does not.
  A failed list read keeps live adds and removals so the retry still merges them.

## Spaces (ADR 0103)

- Keep `app/spaces.rs` as the shared sidebar projection and freshness owner: space roots, leaf rooms, and nonselectable headings/dividers.
  `rooms.selected` owns the open timeline; `spaces.focus` owns a focused header and must never become a message/draft target.
  Reserve a contextual section header above scrolled children when it fits, and keep that root's membership fresh even when the original header is above the viewport.
- Joined spaces come from `RoomDto.room_type`; direct children come from paged `GET …/space/hierarchy?limit=100&enrich=false` and are intersected with the same account's room list.
  Never trigger enrichment or selected-child discovery for sidebar grouping.
  First parent in root order wins independently of collapse state; joined subspaces remain roots.
  Favorites sort within each group, `/filter fav` flattens, and name/unread filters reveal matches temporarily.
  Explicit collapse overrides filter reveal until the filter changes.
  Left collapses the current group and enters the next nonempty group (then Ungrouped); Right expands and enters a focused header's first child.
  Unknown destination membership keeps a keyed navigation intent, canceled by later navigation or filter/account changes.
- Membership fetches acquire one of four permits before spawning, request only visible roots plus lookahead, retain cached membership on failure, and coalesce live invalidations.
  Startup and pending navigation also prioritize their destination root through the same pool.
  A root removed or invalidated during a walk cancels its worker; its request ID fences queued outcomes, including a removal followed by a rejoin.
  Deduplicate room IDs and replace membership only after the final page; a failed or incomplete walk retains the last complete grouping and marks the root incomplete.
  Each walk allows 200 pages, 2 MiB per page, 16 MiB in total, and 60 seconds overall, alongside each read's 30-second timeout.
  Reject malformed, oversized, or repeated cursors and unexpected page/parent IDs.
  Failed walks retry from the first page after 30 seconds; explicit refresh and relationship changes retry immediately.
  Reconnect and `/refresh` invalidate membership and re-read instance order.
- `space_order` is instance-scoped, including `preferences.changed` envelopes with a nil account UUID.
  Preserve hidden/absent keys on reorder, suppress own-device echoes, serialize GET/PUT work, and retain unsaved edits across failures and older reads.
  Do not upload an empty order just because GET returns 404; another client may still have a legacy migration to upload.
  The pin shortcut on a header moves it to the front of the complete instance order, retaining hidden/absent keys and other roots' relative order; it never writes `m.favourite` on a space.
- Launch selection waits for root order and membership, then opens the first projected leaf after the device-state stage applies read markers and drafts.
  A selection deadline or failed read permits a best-effort fallback; explicit navigation cancels the automatic choice.
- Sidebar room activation loads history off the input loop and applies only the current request for the current room.
  Replay live frames observed during that read so an older HTTP snapshot cannot discard new messages, edits, or reaction patches.
  The replay buffer is bounded; overflow leaves the live cache intact and requests a fresh snapshot.

## Event loop and async

- **Reach the first frame before any network await (ADR 0093).** Startup runs
  as spawned stages applied through the main loop's channel, so the client
  paints and accepts keys immediately; a blank terminal during a slow load
  reads as a hang, not a load. Any new startup work joins that chain rather
  than adding a sixth await ahead of the loop. The one deliberate exception is
  the flat-list launch room's timeline, which is bounded and must follow read-marker
  hydration (ADR 0048/0089).
  The pending-invite list is spawned beside the room fetch and does not advance that chain (ADR 0130).
- **Per-room background work is demand-driven and semaphore-bounded (ADR
  0093).** Anything that costs one request per room — space membership reads
  and any future per-room fetch — must ask only for what is on screen
  plus a small lookahead, hold a permit from a bounded pool, and record a
  negative answer so it stops asking. A per-room cooldown bounds repeats of one
  room; it does not bound how many rooms are in flight, which is the property
  that matters on a server with thousands of them.
  Reconnect room/membership reconciliation waits for one second of connection stability and preserves failed reads' retry deadlines.
  Explicit refresh still retries immediately.
  Space membership is also refreshed off screen when an already loaded cache is invalidated: that cache governs ownership and whether an empty root stays hidden.
  These refreshes share the same pool and never queue tasks waiting for permits.
- **Never `await` an API call from key handling or a draw-adjacent path.** Spawn the work as a task and apply its result through the main-loop outcome channel (the lifecycle verbs under _Mutations_ are the model). Blocking the loop freezes input and redraw.
- **Async results must be cancelable or stale-checked before they mutate mode or state.** A result that lands after the user has changed mode, room, or account must be dropped or reconciled against the current state, never applied blindly — the same discipline media workers use for evicted entries.
- **The renderer is read-only over the model (#70).**
  One main-loop iteration is `ui::prepare` then `ui::draw`, both inside the `terminal.draw` callback and both measuring the one `frame.area()` — `Terminal::draw` autoresizes before it yields the frame, so measuring off `terminal.size()` beforehand would let a resize land between the two halves.
  `prepare` settles everything the frame's geometry decides — the three panes' `page_size` and scroll offsets, the message layout cache, a command response that has outgrown the entry box, the open popup's body and clamped scroll — and `draw` paints that and recomputes none of it.
  Anything a key handler or the search paging math reads must be written in `prepare`, never in `draw`: a value the renderer decides is a value the next keystroke reads one frame late, and it only ever worked because the loop happened to paint before it polled for input.
  `draw` still takes `&mut App` for exactly three things, all documented on it — the one-shot render flags it consumes (`clear_media_preview`, `force_terminal_clear`), `prev_image_rects`, and demand-driven media fetch for the thumbnails actually on screen.
  Adding a fourth is a design change, not a convenience.
  Pane geometry is measured once, by `ui::pane_areas` inside `prepare`, and stored in `app.frame.areas` for `draw` to paint into — so the rect that decides a page size is the same object as the rect its rows land in, not a second evaluation that happens to agree.
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
- Enable backup: `POST /v1/accounts/{account_id}/backup/enable`
- Delete: `DELETE /v1/accounts/{account_id}`

Login, logout, and recover return `{ "data": <AccountDto> }`. Recover requires an active account and consumes a transient `recovery_key` without persisting it. Enable backup returns flattened `AccountDto` plus `backup_action`; `recovery_key` is required for create/export/replace and omitted to kick an already-enabled upload. Unverified accounts are refused before the key is sent. Logout is non-destructive: the returned account is `deactivated` and its archive remains available for a later login. Delete returns `204 No Content` and permanently removes the account and its local Axon data.

Login, logout, recover, backup enable, and delete run off the event loop: secret-bearing calls are spawned as tasks that own and then drop the password or recovery key, and results land back through a channel the main loop drains, so the UI keeps redrawing and a second lifecycle verb is refused while one is in flight. Login is idempotent server-side — an already-`active` account is returned unchanged with the password never consulted — so the client reports that no-op distinctly (`already logged in: … (no changes)`) by comparing the returned `account_id` against the accounts that were active before the attempt. After a real new/reactivated login, prompt for a recovery key only when the returned account is not verified; empty Enter or `Esc` skips. Logout asks for `[y/N]` confirmation unless `display.confirm_logout = false`. Delete always asks for an explicit `YES` confirmation.

Login, logout, recover, backup enable, and delete follow the general multi-step interaction rule above: while a lifecycle prompt is open or a request is in flight, background statuses must not overwrite the entry line. Lifecycle outcomes may replace their own status when the operation advances or completes.

The four write operations map to these Axon API endpoints:

- Send: `POST /v1/accounts/{account_id}/rooms/{room_id}/send`
- Edit: `PUT /v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}`
- Redact: `DELETE /v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}`
- React: `POST /v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}/reactions`

All return `{ "data": { "event_id": "..." } }`.

`Mode::Editing`, `Mode::Reacting`, and `Mode::Unreacting` own their respective input flows. Redact fires immediately with no pending state. Unreact uses the redaction endpoint against the current user's reaction event ID; when several distinct reactions exist, Tab cycles the choices before Enter confirms.
A live `m.room.redaction` masks a loaded message, withdraws a loaded reaction, or drops a loaded edit and refetches the original, and is not inserted into the timeline.
One sender stays on a reaction tally until their last known event id for that key is withdrawn.
A frame with no `content.redacts` cannot name its target.
Pre-v11 redactions often put that id only in the top-level `redacts` field, which `/v1/ws` does not carry.
That frame is logged when debug display is on, and the room's loaded timeline is marked stale.
A background read folds `m.replace` rows the way a timeline load does, then copies that snapshot onto the loaded rows, including a body left by a redacted edit, and does not append new rows.
An unloaded target marks only the open room.
A room that is not open keeps its rows until the next time it is loaded.
A search jump clears that in-flight read, the same way a timeline load does, so the old snapshot cannot land on the new slice.

Send media (ADR 0059/0062, `/send <path> [caption]`) is a two-step client call, off the event loop like the other mutations above:

- Stage: `POST /v1/accounts/{account_id}/media/uploads?kind=<image|file>&filename=<name>` — raw bytes as the request body, returns `{ "data": <StagedUploadDto> }` with `upload_id`.
- Send: `POST /v1/accounts/{account_id}/rooms/{room_id}/send-media` — JSON `{ "upload_id", "caption"?, "reply_to"?, "thread_root"? }`, returns `{ "data": { "event_id": "..." } }`.

`kind`/`Content-Type` are inferred client-side from the file extension (`media_kind_and_content_type` in `app.rs`), good enough to satisfy the server's `kind=image` ⇒ `image/*` validation. `/send` reuses `pending_reply`/`pending_thread` exactly like a plain send, and a `media_send_busy` flag (mirroring `lifecycle_busy`) refuses a second `/send` while one is uploading so its status line survives. No optimistic local echo — the sent event arrives over `/v1/ws`.

## Own-message identification

Resolve the account's own Matrix ID through `App::own_user_id_for`, never by reading one source directly.
No single source is reliable, so it tries three in order, and code that consults only one of them gets `None` where another would have answered:

1. `RoomDto.account_user_id`, when the server provides it — this is what makes own-message colouring correct on the first render.
   Keep the field optional in the client DTO, for compatibility with older Axon servers that omit it.
2. `own_senders`, recorded when one of our own events comes back over the WebSocket.
   After `send_message_to_room` succeeds the TUI stores the returned event id as `pending_own_event_id`, and the echo records `account_id → sender`.
   This tier is seeded only by sending a plain message, so a session that has only ever reacted never populates it.
3. The account list, which always carries `user_id` even when the room DTO omits it.

Messages from that sender/account pair render with `colors.own_message_sender` instead of `colors.message_sender`.

Reactions add a fourth tier on top, `own_user_id_from_reaction_rows`, because a badge that is wrong is a visible miscount rather than a colour:
a reaction event already recorded as ours names us in its own raw row.
Both the optimistic local path and the live remote path call it, deliberately — when only one of them had it, the two disagreed about what "ours" meant and a reaction sent from a second device counted the same person twice (#220).

The general rule the reaction bugs taught: **one function answers "is this mine", and every path calls it.**
Two guards over two fields, each settable independently, will disagree the moment one source is unavailable.

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
