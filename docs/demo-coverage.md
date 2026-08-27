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
  `axon-demo-tui --scene <name>` for the TUI, and for web the leading word of a
  test title in `clients/web/e2e/demo/{desktop,mobile}.spec.ts`
  (`pnpm demo --grep rooms`).
- "Not covered" with a reason is worth more than a scene name that does not
  really exercise the thing. Several rows below are deliberately not covered,
  and say why.

**Legend:** scene name = covered · **n/a** = the client has no such capability
(see `docs/client-parity.md`) · **not covered** = the client has it and no demo
shows it.

## Coverage

| Capability                                              | TUI (`axon-demo-tui`) | Web desktop                                                                                               | Web mobile      |
| ------------------------------------------------------- | --------------------- | --------------------------------------------------------------------------------------------------------- | --------------- |
| Room list, with names and topics                        | `rooms`               | `rooms`                                                                                                   | `rooms`         |
| Incoming invite inbox (Accept/Reject)                   | **n/a**               | **not covered** — demo corpus joins invitees through the appservice, so there is no standing-invite scene | **not covered** |
| Room list sort (ADR 0042)                               | `rooms`               | `rooms`                                                                                                   | **not covered** |
| Room list filter: all / DMs / groups                    | `rooms`               | `rooms`                                                                                                   | **not covered** |
| Live room-name filter (Alt-/)                           | `rooms`               | `rooms`                                                                                                   | **not covered** |
| DM title derived from the other member                  | `rooms`               | `rooms`                                                                                                   | `rooms`         |
| Timeline text, sender names, day separators             | `timeline`            | `timeline`                                                                                                | `timeline`      |
| Formatted (HTML) messages with links                    | `timeline`            | `timeline`                                                                                                | `timeline`      |
| Reactions (seeded badges render)                        | `timeline`            | `timeline`                                                                                                | `send`          |
| React to a message, then withdraw it                    | `react`               | `send`                                                                                                    | **not covered** |
| Reaction badge updating live from another user's react  | **not covered**       | **not covered**                                                                                           | **not covered** |
| Replies                                                 | `timeline`            | `timeline`                                                                                                | `send`          |
| Edits (`m.replace`, applied in place)                   | `threads`             | `threads`                                                                                                 | **not covered** |
| Redactions (`[redacted]` tombstone)                     | `threads`             | `threads`                                                                                                 | **not covered** |
| In-timeline find (Ctrl-F, n/N)                          | `threads`             | **n/a**                                                                                                   | **n/a**         |
| Threads: open panel, read replies                       | `threads`             | `threads`                                                                                                 | `threads`       |
| Inline images (Sixel / Kitty / iTerm2)                  | `media`               | **n/a**                                                                                                   | **n/a**         |
| Full-size image preview                                 | `media`               | `media`                                                                                                   | `media`         |
| Image galleries (adjacency grouping, ADR 0081)          | **n/a**               | `media`                                                                                                   | `media`         |
| Lightbox paging across a gallery run                    | **n/a**               | `media`                                                                                                   | `media`         |
| Full-text search across rooms (Tantivy)                 | `search`              | `search`                                                                                                  | `search`        |
| Search field filters (`room:`, `all:true`)              | `search`              | `search`                                                                                                  | `search`        |
| Search hit as a deep link into a room                   | **not covered**       | `search`                                                                                                  | `search`        |
| Jump to a date in history                               | `jump`                | **not covered**                                                                                           | **not covered** |
| Sending a message                                       | `send`                | `send`                                                                                                    | `send`          |
| Deleting one's own message                              | **not covered**       | `send`                                                                                                    | `send`          |
| Shortcuts and help popups                               | `shortcuts`           | `shortcuts`                                                                                               | **n/a**         |
| Spaces: picker, filtering, hierarchy (ADR 0084)         | **n/a**               | `spaces`                                                                                                  | `rooms`         |
| Single-pane navigation and Back (ADR 0062)              | **n/a**               | **n/a**                                                                                                   | `rooms`         |
| Room information panel                                  | **not covered**       | `rooms`                                                                                                   | `timeline`      |
| Unread thread picker (Alt-T)                            | **not covered**       | **not covered**                                                                                           | **not covered** |
| Search result sort / group / edit toggles               | **not covered**       | **not covered**                                                                                           | **not covered** |
| Sending media                                           | **not covered**       | **not covered**                                                                                           | **not covered** |
| Message actions: edit, redact, reply                    | **not covered**       | **not covered**                                                                                           | **not covered** |
| Room actions: invite, leave, pin (M19)                  | **not covered**       | **not covered**                                                                                           | **not covered** |
| Device verification (SAS)                               | **not covered**       | **not covered**                                                                                           | **not covered** |
| Megolm key backup snapshot and enable (ADR 0098)        | **not covered**       | **not covered** — `/accounts` is not a demo scene                                                         | **not covered** |
| Matrix OAuth QR account acquisition                     | **n/a**               | **not covered** — demo stack has no MAS or second trusted device                                          | **not covered** |
| Typing indicators and read receipts (M18)               | **not covered**       | **not covered**                                                                                           | **not covered** |
| Inline image whose terminal encode failed (placeholder) | **not covered**       | **n/a**                                                                                                   | **n/a**         |
| Debug overlay diagnostics (`display.debug`)             | **not covered**       | **n/a**                                                                                                   | **n/a**         |
| Room list loading state during startup                  | **not covered**       | **not covered**                                                                                           | **not covered** |
| State-event notices (ADR 0083)                          | **not covered**       | **not covered**                                                                                           | **not covered** |

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
  follow: script the _undo_ as well, so the scene leaves the room as it found it
  and its assertions stay honest on a second run — a react-only scene passes
  vacuously the second time, on the badge the first run left behind.
- **Device verification.** Needs a second device to verify against, which the
  corpus does not stand up.
- **Matrix OAuth QR account acquisition.** Needs MAS and a second trusted
  Matrix device; the demo stack provides neither.
- **Room list loading state.** The panel names the startup stage it is on
  (ADR 0093) while accounts, rooms, and device state load. Against the demo
  stack all three land in well under a frame, so a scene would either catch
  nothing or have to fake a slow server. Worth revisiting if the demo ever
  runs against a seeded large-room corpus, which is also what would make it
  worth showing.
- **Failed inline encode, and the debug overlay.** Both are states a _healthy_
  run never reaches. The placeholder needs an image the terminal-graphics
  encoder rejects, or a protocol cache saturated with in-flight encodes; the
  overlay needs `display.debug = true`, which no scene sets. Covering the first
  honestly means teaching the corpus to serve a deliberately undecodable image,
  not writing a longer scene. The overlay is closer to a deliberate omission: it
  is a diagnostic surface, and a demo of it would show a screen no user is meant
  to see. Both are TUI-only — the web renders images natively with no encode
  step (see the inline-images row) and has no equivalent overlay — hence `n/a`
  rather than a gap.
- **Typing indicators and read receipts, and a reaction badge updating live.**
  All need a second live client acting concurrently; the corpus is seeded history, not a running participant.
  The reaction case is worth distinguishing from the two rows above it:
  `timeline` covers badges that were already in the aggregate at load, and `react` covers this client reacting to itself.
  Neither exercises an `m.reaction` frame arriving over the WS for someone else's reaction, which is a different code path — the frame patches the target message's aggregate rather than being rendered as a row of its own.
- **Spaces and galleries in the TUI.** The TUI has neither (see
  `docs/client-parity.md`); spaces appear in its room list as ordinary rooms.
  These are `n/a` rather than gaps in the demo.
- **Sort, filter, and the name filter on web mobile.** The controls are all
  there and all work; they are simply the same demonstration as the desktop
  take, on a third of the screen. The mobile scenes spend their budget on what
  only mobile has — the single-pane transition, Back, the room-information
  panel, and touch-sized message actions. Worth revisiting if the mobile room
  list ever diverges from the desktop one.
- **Edits and redactions on web mobile.** Same reasoning: the rendering is
  identical to the desktop take, which covers it.
- **Jump to a date on web.** The client has no date-jump entry point of its own
  — the TUI's `/jump` has no web counterpart yet — so this is closer to a
  parity gap than a demo gap; see `docs/client-parity.md`.
- **State-event notices (ADR 0083).** Deliberately switched _off_ for both web
  recordings. `/createRoom` does not honour a backdated `ts`, so the newest
  event in every corpus room is the viewer's own join; at the shipped default
  every timeline would end on "Alex Marx joined the room", which reads as a
  broken client. Covering this honestly needs a corpus that ends each room on a
  message, not a longer scene.
- **In-timeline find on web.** The web client has no Ctrl-F of its own; search
  is the answer there, which the `search` scenes cover.

## Findings the demo scenes pinned

Things that are true of the clients, discovered by scripting them, and which a
scene now depends on:

### TUI

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

### Web

- **Search scope is the point of invocation** (ADR 0066). Opened from inside a
  room, an unqualified query searches _that room_ — so a scene that means to
  show cross-room search has to say `all:true`, and one that searches for a word
  living in another room finds nothing at all. Both web `search` scenes are
  built around this rather than around it.
- **A room-list row's rendered text contains a ticking relative timestamp**
  (`1m` → `2m`). Comparing whole-row text across a scene reports a reorder that
  never happened; the sort step compares `href` instead.
- **The overlay's result count is the server's total; `a.search-hit` is not.**
  The list auto-pages on scroll, so counting rendered hits measures where the
  list happens to be scrolled to.
- **A redaction leaves a permanent tombstone row.** "Script the undo" keeps the
  _content_ out of the next run but cannot remove the row, so two mutating
  scenes must not share a room — whichever runs second opens on the other's
  "message deleted" — and a real take is recorded against a freshly seeded
  stack.
- **A touch tap still emits compatibility mouse events.** The demo's pointer
  overlay gates on `pointerType`, or the phone recording grows a desktop arrow
  cursor hovering beside the finger.
- **Mobile WebKit has no mouse wheel**, which is the correct answer — a phone
  has none either. The mobile timeline scene scrolls with a smooth-behavior
  `scrollBy` rather than pretending to a gesture it is not.
- **Message actions are hidden until the row is tapped** at phone widths, and
  Enter is not the send affordance a touch-only user has — the Send button is.
