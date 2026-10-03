# ADR 0078 — Web client keyboard shortcuts

## Context

The web client has no keyboard shortcuts. Its only key handling is the
composer's Enter/Shift-Enter/Escape and the spoiler span's Enter/Space; there
is no document-level listener anywhere. ADR 0046's open question 7 deferred the
question with a recommendation: "a fixed core set for MVP (room nav, compose
focus, search, reply/edit on selection); rebindability later," with the full
parity audit at M-W11.

The obvious move is to mirror the TUI, which is keyboard-everything. It does
not survive contact with a browser.

- **The TUI's room-list chords are all `Alt`-modified** (ADR 0042): `Alt-F`
  cycle filter, `Alt-S` cycle sort, `Alt-D`/`Alt-G`/`Alt-V`/`Alt-0` set a
  category, `Alt-/` name filter. That ADR states the reason — "alt-modified so
  unmodified characters still reach the compose box" — which is a consequence
  of a terminal having exactly one focused pane. In a browser `Alt-F` opens
  Chrome's menu and `Alt-D` focuses the address bar.
- **The TUI's room navigation is `Ctrl-N`/`Ctrl-P`**, which a web page cannot
  intercept at all (new window, print).
- **`Ctrl-K` in the TUI is `message_up`**, not a jump-to-search. The TUI's find
  is `Ctrl-F`. On the web, `Ctrl-K` is the near-universal "jump to filter or
  command palette" (GitHub, Slack, Linear).
- **The TUI has no Slack-style "Up edits the last message."** `Up` in its
  composer *selects* the previous message and moves focus to the timeline;
  editing is then `e`. That select-then-act model rests on bare letters being
  actions, which requires the single-focused-pane assumption.

So literal chord parity is not available. Semantic parity is.

## Decision

- **Keep the TUI's semantics, choose browser-safe chords.** The cycle orders are
  imported wholesale: filter `all → dms → groups → unread → favorites`
  (matching `RoomFilter::CYCLE`), sort `recent → oldest → az → za` (matching
  `RoomSort::next`), both forward-only and wrapping, with the name filter
  outside the cycle as in ADR 0042. `ROOM_FILTERS`/`ROOM_SORTS` become exported
  from the settings store so the shortcut and the store cannot disagree.

  | Chord | Action | TUI equivalent |
  |---|---|---|
  | `Ctrl-K` | Filter rooms by name | `Alt-/` |
  | `↑`/`↓`, `Enter` | Move through and open rooms | same |
  | `Ctrl-↑`/`Ctrl-↓` | Previous / next room | `Ctrl-P`/`Ctrl-N` |
  | `Ctrl-Shift-F` | Cycle filter | `Alt-F` |
  | `Ctrl-Shift-S` | Cycle sort | `Alt-S` |
  | `Ctrl-B` | Show/hide the room list | `Alt-R` |
  | `↑` (empty composer) | Edit your last message | *none* (`Up` then `e`) |
  | `Escape` | Close panel, then return to composer | same (staged) |
  | `?` or `Ctrl-/` | Show the shortcut list | help popup |

- **`mod` means Ctrl or Cmd.** `chordOf` folds `metaKey` into `ctrlKey`, so
  macOS gets `Cmd-K` for free without a second table.

- **Arrow navigation roves real DOM focus** across the `a.room-link` anchors
  rather than tracking a selected index. `Enter` then activates the anchor
  natively, screen readers announce each room as it is reached, and `ArrowUp`
  off the top returns to the filter input where the sequence began. No
  `aria-activedescendant`, no new selection state.

- **`Up` in the composer edits the last own message**, Slack-style, when the
  composer is empty and no reply/edit is already in progress. This is a
  deliberate divergence from the TUI: the web has no timeline cursor, and
  adding one to support select-then-act is a separate, larger change. The
  editability rule is extracted as `isEditable()` and shared with `EventRow`,
  so the message `Up` picks is exactly one the Edit button would offer.

- **Escape is staged, resolved by `preventDefault`.** Handlers ignore an event
  that is already `defaultPrevented`, so precedence falls out of who claims it
  first rather than any handler knowing about the others: a modal claims it in
  the *capture* phase (independent of mount order), the focused composer claims
  it to cancel its own reply/edit banner, and whatever survives lands in
  `RoomPage` — cancel the banner, else close the thread panel, and either way
  return focus to the composer. The TUI's two-stage Escape. This also gives
  `EditHistory`, a `role="dialog"` with no Escape handler until now, the
  close-on-Escape it always should have had.

  `RoomPage` cancels the banner *as well as* the composer, which looks
  redundant and is not: focus sits on `<body>` for a beat while the composer
  remounts into edit mode, and an Escape in that window would otherwise skip the
  banner and close the thread panel behind it.

- **Focus returns to the composer through a `composerFocus` counter signal** on
  the service graph, alongside `activeRoom`. The room list cannot reach the
  open room's textarea, and a DOM query would find the thread panel's composer
  too. The thread's composer simply does not take the prop.

- **`SHORTCUTS` is one exported table** that both documents and renders the help
  popup. The TUI must keep `popup_shortcuts_lines` in sync with `keymap.rs` by
  hand; here the popup renders the table directly, so it cannot drift from what
  is bound. (The table is still hand-mapped to the handlers — a genuine binding
  registry is possible later, and is what rebindability would need.)

- **Bare-character chords are withheld while typing.** `useShortcuts` skips
  input, textarea, select and contenteditable targets unless a binding opts in
  with `whileTyping`, which every modifier chord does. Without this, `?` could
  never be typed into a message.

- **Help therefore needs a modifier twin.** `?` alone made the shortcut list
  unreachable from the composer, which autofocuses and is where the cursor
  usually sits — the one place a user would reach for help. `Ctrl-/` (Slack's
  `Cmd-/`) opens it too and survives a text field. Every spelling is bound:
  `Ctrl+Shift+/` reports `event.key === '?'` on a US layout and `/` on others,
  so `mod+shift+/` alone would silently never fire. A visible `?` button in the
  topbar makes the shortcuts findable without knowing a shortcut at all.

- **Controls advertise their own chords.** The sidebar toggle, the filter chips,
  the sort select and the name filter carry a `title` tooltip and
  `aria-keyshortcuts`. These read from an exported `KEYS` table that also feeds
  `SHORTCUTS`, so a chord cannot be renamed in one place and left stale in
  another — the same anti-drift rule the popup follows. `aria-keyshortcuts` sits
  on the filter *group*, not on each chip: `Ctrl-Shift-F` cycles the group
  rather than activating any one button, and claiming otherwise would mislead
  assistive tech.

- **The open room is marked with `aria-current="page"`**, which is both the
  state and the CSS hook, so assistive tech and the eye read the same attribute.
  Because `.room-link:hover` is `--bg-raised` — a neutral lightness shift — the
  selection cannot be a mere raised background or a hovered row would look
  selected. It is an accent-tinted background (a *hue* shift; measured at only
  ~1.1:1 luminance against hover), a 3px inset accent bar, and a semibold name.
  The bar and the weight are load-bearing, not decorative: hue alone would fail
  a color-blind user. An inset `box-shadow` rather than a `border-left`, so the
  row does not shift 3px when it becomes current.

- **`.room-link` gets a `focus-visible` ring.** Arrow keys rove focus through
  these anchors, and `.room-list` clips overflow, so the browser default outline
  was invisible. The ring is inset and distinct from the selected row, since a
  room can be focused and selected at once.

## Consequences

- Chord parity with the TUI is explicitly abandoned; users of both clients must
  learn two sets for the room list. The help popup says so. The cycle *orders*
  match, so the mental model transfers even when the keys do not.

- `Ctrl-K`, `Ctrl-B` and `Ctrl-/` are intercepted from the browser. All three
  are widely overridden by web apps and all three are cancellable;
  `Alt-F`/`Ctrl-N` would not have been, which is the whole reason for the
  divergence.

- The `whileTyping` guard means **any** future bare-character chord is
  unreachable from the composer, which is where focus lives. `?` proved this the
  hard way. A bare chord is now only acceptable for something a user would never
  want while typing, and anything they might want needs a modifier twin.

- The selected row and the keyboard-focused row are different states that can
  coexist, so they must stay visually distinct. Restyling either one has to
  keep the other legible on top of it.

- `Up`-to-edit means a user who wants to scroll the timeline from a focused,
  empty composer cannot do it with `Up`. It fires only on an empty composer
  with no banner, which is the standard guard.

- Escape ordering depends on handlers calling `preventDefault`. A future
  handler that forgets will silently double-fire. The `defaultPrevented` guard
  in `useShortcuts` makes the convention cheap to follow but does not enforce
  it.

- `Ctrl-K` focuses the filter from a `setTimeout`, not directly from the effect:
  it may first have to un-collapse the sidebar or leave a utility page, and
  `focus()` on a `display: none` element is silently a no-op.
  `requestAnimationFrame` is *not* a safe substitute — a page that is not
  painting (a background tab, a headless browser) may not run it for hundreds
  of milliseconds.

- `useShortcuts` subscribes its `document` listener once and reads the current
  bindings through a ref. Re-subscribing per render looks equivalent and is not:
  Preact flushes effects after paint, so a key pressed in that window runs the
  previous render's closure over stale state. `Ctrl-B` toggled the sidebar
  closed and then refused to reopen it until this was fixed — any state a
  handler reads from render scope is exposed to the same trap.

- No rebindability, matching ADR 0046's recommendation. `KEYS` and `SHORTCUTS`
  are the seam where a keymap would attach — a rebind would have to update the
  tooltips and `aria-keyshortcuts` with them, which is exactly why they read
  from `KEYS` rather than repeating literals.

- Deferred: a timeline cursor and the TUI's select-then-act message keys
  (`r`/`e`/`d`/`t`), `Ctrl-F` search (the search UI is M-W10), pin/unpin from
  the keyboard, and next/prev-unread (which the TUI does not have either).

- The Tauri shell additionally binds the platform-standard chords this ADR
  had to avoid (`Ctrl-F`/`⌘-F`, `Ctrl-N`/`⌘-N`, `Ctrl-,`/`⌘-,`, `F1`/`⌘-?`);
  see ADR 0107.
