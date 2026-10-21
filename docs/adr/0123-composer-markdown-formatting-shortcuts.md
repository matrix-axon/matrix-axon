# ADR 0123 — Markdown formatting shortcuts in the composer

Status: accepted

## Context

The composer is a plain `<textarea>` and messages are sent as Markdown. Users
who think in "make that bold" have to type the markers by hand. Every chat app
answers `Ctrl-B`/`⌘-B`, `Ctrl-I`/`⌘-I` and friends, so their absence reads as a
missing feature.

Two constraints shaped the design:

- **The chords overlap existing ones.** ADR 0078 binds `Ctrl-B` app-wide to
  "show/hide the room list", and it fires from inside the composer. `Ctrl-K` is
  the room filter. Plain `Ctrl-X` is cut.
- **A textarea has one undo stack, and assigning `value` clears it.** Wrapping
  a selection by rewriting the value would make Ctrl-Z skip back over
  everything the user typed before.

## Decision

The composer answers these chords. `mod` is `⌘` on macOS/iPadOS and `Ctrl`
elsewhere, and only that modifier counts (`isPrimaryModifier`), so macOS
`Ctrl-B`/`Ctrl-I` stay Emacs cursor keys and Windows AltGr (Ctrl+Alt) keeps
typing characters.

| Format        | Chord         | Markdown      |
| ------------- | ------------- | ------------- |
| Bold          | `mod-B`       | `**text**`    |
| Italic        | `mod-I`       | `*text*`      |
| Strikethrough | `mod-Shift-X` | `~~text~~`    |
| Inline code   | `mod-E`       | `` `text` ``  |
| Link          | `mod-Shift-K` | `[text](url)` |

The same chords are used in the browser and in the native shell. Chrome and
Firefox let a page override these keys (as ADR 0078 already relies on for
`Ctrl-K`); the keys a browser reserves outright (`Ctrl-N/T/W` and friends) are
not used. Strike and link take Shift because their unshifted forms are cut and
the room filter. If a browser turns out to claim one of them, the fix is a
`mod-Shift` variant for that chord on the web only, via the `native` field of
`KEYS` that ADR 0107 introduced.

**Sidebar toggle.** `mod-B` is bold while the composer has focus. The
composer's handler runs first and calls `preventDefault()`, and the app-wide
handler skips a prevented event, so outside the composer `mod-B` still toggles
the room list. A new twin, `mod-Shift-B`, toggles it from anywhere including the
composer. The help popup lists both; the toggle's tooltip keeps `Ctrl-B`.

**Behavior.**

- With a selection, a wrapping chord wraps it and keeps it selected, so a second
  press unwraps. It unwraps whether the markers sit just outside the selection
  or inside it.
- With no selection it inserts an empty pair with the caret between. Pressing it
  again in the empty pair removes it.
- Markers hug the text. Leading and trailing whitespace in the selection stays
  outside them, because `**hello **` does not render as bold. A selection of
  only whitespace acts as a caret.
- Code sizes its delimiter to the content: the shortest backtick run that no run
  inside matches, so `a`b` becomes ``` ``a`b`` ```, and content that begins or
  ends with a backtick gets the padding space Markdown strips. Unwrapping
  removes the whole delimiter, padding included.
- Bold and italic share `*`, so they count a run of asterisks: one is italic,
  two bold, three both. Each can be added to or taken off the other.
- Link wraps the selection as the label and selects the `url` placeholder so
  the URL can be typed straight over it. With no selection it inserts
  `[link](url)`. Pasting a URL over a selection already makes a link; the chord
  is for typing the URL by hand.

**Undo.** The edit is a single minimal replacement applied with
`document.execCommand('insertText')`, which the browser records as typing, so
Ctrl-Z undoes exactly the formatting. `execCommand` is deprecated but remains
the only programmatic edit that preserves undo. Where it is missing or refuses
(jsdom), the fallback is `setRangeText` plus a dispatched `input` event.

**Discoverability.** The chords are in `KEYS` and the "Messages" group of
`SHORTCUTS`, so the help popup, platform labels (`⌘` on macOS) and ARIA key
names come from the one table.

## Consequences

- `composer-formatting.ts` is pure and unit-tested; the composer only maps the
  event to an edit and applies it.
- The sidebar toggle gains a second chord, and its plain chord no longer works
  from the composer. This is the one behavior change to an existing shortcut.
- Not verified in real browser chrome: a test runner delivers the key straight
  to the page, so it cannot show that a browser does not claim a chord first.
  That needs a manual pass in Chrome, Firefox and Safari, and in the macOS and
  Windows shells.
- The TUI composer is unchanged.
