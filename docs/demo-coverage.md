# Demo Coverage

A human-maintained tracker for one recurring failure mode: a capability that
took a milestone to build never reaches a demo, so it is invisible twice — it
does not appear in the videos anyone evaluating the project watches, and it is
never exercised by the demo drivers that would notice it breaking.

This is the mitigation ADR 0086 names for the fact that the videos are
regenerated **by hand** and can therefore go stale. It is review discipline, not
an enforced check. If it proves leaky, the ADR's cheapest escalation is a CI
check that a PR touching a client render path also touches this table.

This doc is not auto-generated, and it is not `docs/client-parity.md`. Parity
answers "does this client expose the capability at all?"; this answers "does a
demo actually show it, and does a driver actually walk it?" A row can be **Done**
in parity and **not covered** here.

## How to maintain this

- Update the row in the **same PR** that changes what a client renders — per
  `AGENTS.md`'s `docs/` cross-silo exception, this file can land alongside a
  change in any one silo.
- A cell names the **scene** that covers it, so the claim is checkable:
  `axon-demo-tui --scene <name>` for the TUI, a Playwright `demo-desktop` /
  `demo-mobile` spec name for web.
- "Not covered" with a reason is worth more than a scene name that does not
  really exercise the thing. Several rows below are deliberately not covered,
  and say why.

**Legend:** scene name = covered · **n/a** = the client has no such capability
(see `docs/client-parity.md`) · **not covered** = the client has it and no demo
shows it.

## Coverage

Web columns are empty because the web recording is ADR 0086 phase 3 and has not
landed. They are listed now so the gap is visible rather than discovered later.

| Capability | TUI (`axon-demo-tui`) | Web desktop | Web mobile |
|---|---|---|---|
| Room list, with names and topics | `rooms` | — | — |
| Room list sort (ADR 0042) | `rooms` | — | — |
| Room list filter: all / DMs / groups | `rooms` | — | — |
| Live room-name filter (Alt-/) | `rooms` | — | — |
| DM title derived from the other member | `rooms` | — | — |
| Timeline text, sender names, day separators | `timeline` | — | — |
| Formatted (HTML) messages with links | `timeline` | — | — |
| Reactions (seeded badges render) | `timeline` | — | — |
| React to a message, then withdraw it | `react` | — | — |
| Replies | `timeline` | — | — |
| Edits (`m.replace`, applied in place) | `threads` | — | — |
| Redactions (`[redacted]` tombstone) | `threads` | — | — |
| In-timeline find (Ctrl-F, n/N) | `threads` | — | — |
| Threads: open panel, read replies | `threads` | — | — |
| Inline images (Sixel / Kitty / iTerm2) | `media` | — | — |
| Full-size image preview | `media` | — | — |
| Image galleries (adjacency grouping, ADR 0081) | **n/a** | — | — |
| Full-text search across rooms (Tantivy) | `search` | — | — |
| Search field filters (`room:`) | `search` | — | — |
| Jump to a date in history | `jump` | — | — |
| Sending a message | `send` | — | — |
| Shortcuts and help popups | `shortcuts` | — | — |
| Spaces: picker, filtering, hierarchy (ADR 0084) | **n/a** | — | — |
| Unread thread picker (Alt-T) | **not covered** | — | — |
| Search result sort / group / edit toggles | **not covered** | — | — |
| Sending media | **not covered** | — | — |
| Message actions: edit, redact, reply | **not covered** | — | — |
| Room actions: invite, leave, pin (M19) | **not covered** | — | — |
| Device verification (SAS) | **not covered** | — | — |
| Typing indicators and read receipts (M18) | **not covered** | — | — |

## Why the uncovered rows are uncovered

- **Unread thread picker.** Needs a room the viewer has fallen behind in. The
  corpus makes the viewer a member of every room from before its first message,
  so nothing is unread and the picker is empty. Covering it means teaching the
  corpus to leave a room unread, not writing a longer scene.
- **Search result sort / group / edit toggles.** Each reloads asynchronously and
  restores the result view when it lands, so an Esc that follows one can be
  undone by it. The starting sort order is also not fixed between runs, so the
  toggled label cannot be waited on. A scene here would be a race, and a flaky
  scene is worse than an honest gap.
- **Sending media, the remaining message actions, room actions.** These mutate
  the world. They are safe against the disposable local stack and worth adding;
  they are simply not written yet. `react` shows the pattern the rest should
  follow: script the *undo* as well, so the scene leaves the room as it found it
  and its assertions stay honest on a second run — a react-only scene passes
  vacuously the second time, on the badge the first run left behind.
- **Device verification.** Needs a second device to verify against, which the
  corpus does not stand up.
- **Typing indicators and read receipts.** Need a second live client acting
  concurrently; the corpus is seeded history, not a running participant.
- **Spaces and galleries in the TUI.** The TUI has neither (see
  `docs/client-parity.md`); spaces appear in its room list as ordinary rooms.
  These are `n/a` rather than gaps in the demo.

## Findings the demo scenes pinned

Things that are true of the clients, discovered by scripting them, and which a
scene now depends on:

- `/room` resolves a room id, a canonical alias, or a room **name**. A DM has no
  name — its "Maya Harrison" title is derived per render from the other member —
  so `/room Maya Harrison` answers "room not found" and the `send` scene
  addresses the DM by id.
- The search result view binds bare letters to its own actions (`s` sort, `g`
  group, `e` edit, `r` reply, `t` thread). A command typed while it has focus is
  not typed at all: it is read as that sequence of shortcuts. Esc first.
- Shift-J on an empty input opens the date-jump prompt, by design. Scripted
  message text must not begin with a capital J.
- The newest event in every corpus room is the viewer's own join, because
  `/createRoom` does not honour a backdated `ts`. Anything that selects "the
  last message" lands on that join, not on the last message.
- Graphics are scaled by `cells × cell size`, so the pilot must know the
  terminal's true cell size in pixels. `TIOCGWINSZ` reports zeros on many
  terminals; the answer has to come from an XTWINOPS query, which is why the
  pilot asks on the child's behalf. Guessing renders every image smaller than
  the frame around it — subtle enough to reach a finished recording.
- Sending a reaction returns the input to compose mode, so the Shift-U withdraw
  chord is typed as a bare `U` into the buffer rather than acting on the
  message. The `react` scene uses `/unreact`, which acts on the selection.
