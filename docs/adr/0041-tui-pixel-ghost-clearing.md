# ADR 0041 — TUI pixel ghost clearing on pane content transitions

> **Numbering note.** This ADR was originally drafted as 0040 and renumbered to
> 0041 to avoid colliding with the cross-user SAS verification ADR (0040). Some
> earlier TUI commit messages — notably in the messaging-UX work merged via
> PR #155 (e.g. "clear pixel ghosts on pane transitions … (ADR 0040)") — refer
> to this work as "ADR 0040". Those are stale references to this document, now
> numbered 0041.

## Status

Accepted

## Context

The TUI message pane renders image thumbnails using ratatui-image in either
halfblock (Unicode ▄▀ characters) or sixel/iTerm2 protocol.  When the pane
content changes wholesale — opening or closing a thread panel, switching rooms,
or changing the account filter — stale pixel data from the previous view can
persist as a "ghost" overlaying the new content.

The root cause differs by protocol:

* **Sixel / iTerm2** — pixel data is written directly to the terminal as escape
  sequences, completely outside ratatui's cell model.  ratatui's diff renderer
  never emits terminal codes for those positions unless a buffered cell actually
  changes, so the pixels are never erased.

* **Halfblock** — characters are in ratatui's buffer, but the `messages_background`
  color defaults to `Color::Reset`.  Cells that were under an image in the
  previous frame carry `bg = Reset, skip = true`; cells at those same positions
  in the new frame carry `bg = Reset, skip = false`.  Because `Cell::PartialEq`
  in ratatui 0.30 includes the `skip` field, these cells *should* compare as
  unequal — but in practice the empty cells below the new (shorter) content also
  have `bg = Reset` from the buffer reset, so there is no effective difference
  for cells that simply have no new content at all.

## Decision

When the message pane content changes wholesale, set `App::force_terminal_clear`
to `true`.  The `draw()` function checks this flag after all message-pane
widgets have finished rendering and applies `CellDiffOption::AlwaysUpdate` to
every cell in `messages_area` whose symbol is a plain space (`" "`).

Restricting `AlwaysUpdate` to eligible blank cells is critical:

* **Blank cells with `diff_option == None`** are the ones the diff would
  otherwise skip (no visible change from the previous frame), so forcing
  emission clears any stale pixel data underneath them.

* **Blank cells with `diff_option == Skip`** must NOT receive `AlwaysUpdate`.
  Sixel and Kitty image widgets set `CellDiffOption::Skip` on every cell they
  occupy (their actual pixel data is written directly to stdout as escape
  sequences).  Overwriting `Skip` with `AlwaysUpdate` causes ratatui's diff to
  emit a space character at the cursor position directly over the just-rendered
  image, destroying it.  Ghost cells from a *previous* frame carry
  `diff_option == None` (the per-frame buffer-reset default) and are therefore
  safe to mark.

* **Non-blank cells** (halfblock ▄▀ chars, text) are already caught by the
  normal diff because their symbol changed.  Force-emitting them is harmful:
  halfblock characters have "East Asian Ambiguous" width, so some terminals
  advance the cursor by 2 columns when rendering one.  ratatui assumes 1-column
  width and skips `MoveTo` for the next adjacent cell; the drift accumulates
  across a row and displaces the right border character, producing a staggered
  border.

`force_terminal_clear` is set in three places:

| Trigger | Location |
|---|---|
| Thread panel opens | `open_thread_panel` in `app/relations.rs` |
| Thread panel closes | `close_thread_panel` in `app/relations.rs` |
| Room / account switch | `load_selected_timeline` in `app/rooms.rs` |

## Alternatives considered

**`frame.render_widget(Clear, messages_area)`** — sets all cells to default
before the Paragraph renders.  Ineffective: `Clear` produces `bg = Color::Reset`
cells, identical to the reset-buffer default; the diff detects no change and
emits no terminal codes.

**`terminal.clear()`** — resets the entire previous buffer, forcing a full-screen
repaint on the next draw.  Works, but emits terminal codes for every cell on
screen at once.  The burst includes halfblock chars without interleaved `MoveTo`
calls, causing the same cursor-drift / staggered-border problem described above.

**Row-by-row crossterm erase (`ClearType::UntilNewLine`)** — erases terminal
cells directly, bypassing ratatui's buffer.  The previous buffer still shows
old content, so ratatui's diff sees "no change" and never redraws the border
characters, causing them to disappear.

## Consequences

* Ghost pixels are cleared on every pane transition without a full-screen
  repaint and without staggering borders.
* Any future code that changes the message pane content wholesale (e.g. a new
  search-results view) must set `force_terminal_clear = true` to get the same
  behavior.
* If ratatui changes its `Cell::PartialEq` semantics or adds a first-class
  "force repaint region" API, the `AlwaysUpdate` workaround can be removed.

## Related rendering constraints discovered during implementation

**`[thread root]` label must be included in first-body-width accounting.**
The `[thread root] ` label (14 columns) is injected between the sender name
and the first body span on the header line.  `first_body_width` must subtract
this amount when computing how many columns the body text may occupy on that
line.  Failing to do so causes the body to overflow the right border, which
overwrites the border character and produces a staggered-border appearance.

**Thread badges, reply context, and reactions must be wrapped to content
width.**  These spans are appended as single `Line` values.  If the text
exceeds `continuation_body_width` (= inner panel width − 2 for the indent),
the overflow character lands on the border column, replacing `│` with text.
The border is re-rendered on every subsequent frame but the Paragraph
overwrites it again, so the border consistently appears missing at those rows.
All three element types now pass through `wrap_rich_lines` to stay within the
safe content width.

**East Asian Ambiguous characters must be measured with CJK widths.**  Characters
such as `·` (U+00B7 MIDDLE DOT) and `■` (U+25A0 BLACK SQUARE) have Unicode
"East Asian Ambiguous" width.  `unicode_width::width()` returns 1 for them, but
many terminals render them as 2 columns.  `wrap_rich_lines` uses
`char::width_cjk()` so it wraps these characters conservatively, preventing a
1-column overflow into the border column.
