# ADR 0066 — Web client message search (M-W10)

## Context

The web client cannot search. The server side has been done since ADR 0039:
`GET /v1/search` runs a BM25-ranked Tantivy query with optional narrowing by
account, room, sender substring, and time bounds, returning ranked hits as
fully hydrated events behind an opaque cursor. The endpoint is already present
in the generated `schema.d.ts` — it has simply never been called. ADR 0046
scoped this milestone (M-W10) as "UI over `GET /v1/search`, results with
room/account context, deep-link into the room at the hit via `?event=`", and
the `?event=` jump-and-reveal contract it depends on landed with the deep-link
work — `RoomPage`'s jump effect carries a comment saying M-W10's search results
navigate this way.

The TUI already has `/search` (ADR 0047): a `field:value` token grammar, a
guided form, and a paged results overlay. Its _semantics_ are the right prior
art — the scope model, the filter set, client-side sorting — but its shape is a
terminal's. A browser has a URL bar, a Back button, shareable links, and users
who expect Slack/GitHub-style search: a box summoned from anywhere, visible
filter affordances, and results that behave like navigation.

Two API facts shape the client design. The server returns **no snippets, no
term highlights, and no surrounding context** — ADR 0047 explicitly made
context a client concern. And search can be **disabled server-side**
(`search.enabled = false` → `503`), so an unavailable state is a first-class
outcome, not an error.

## Decision

### An overlay, addressed by the URL

Search is a modal overlay mounted by `ShellChrome`, following the existing
modal contract (`useModalFocus`, `overlay`/`overlay-panel`, capture-phase
Escape). But unlike the help popup, its open/closed state and its query are not
component state — they live in the URL:

- `?search=<token string>` on the **current route**. The param's presence
  (even empty) opens the overlay; its value is the query in the token grammar
  below. `&ssort=newest|oldest` carries a non-default sort. No cursor is
  serialized; a shared or restored link re-runs page one.
- Opening pushes a history entry; subsequent submits **replace**, so a
  search session is one Back-press to leave, not one per refinement.
  Escape and the close button strip the param via `route()`.

A dedicated `/search` route was rejected: routing away would unmount the room
page under the overlay, discarding timeline scroll position and composer state.
A query param keeps the page mounted — query-only changes already do not
remount `RoomPage` (WCR-09) — while still making every search shareable and
Back-button-native. The asymmetry (Escape after following a shared search link
navigates to the underlying page) is the correct reading of "close".

Clicking a result routes to `/{accountId}/rooms/{roomId}?event={eventId}`.
That closes the overlay (the `search` param is gone) and rides the existing
jump + reveal + highlight machinery unchanged. Search adds no timeline
plumbing.

### Chips and tokens are the same thing

The input accepts the TUI's `field:value` tokens, and the UI renders the
parsed filters as removable chips (scope, sender, date range) with dropdown
affordances for the pointer-first user. A completed token typed into the box
becomes a chip; removing a chip re-serializes the remainder. One pure module —
`search-tokens.ts` — is simultaneously the parser, the chip model, the URL
serialization, and the API-parameter mapping, so the three surfaces cannot
drift.

The grammar is a deliberate subset of the TUI's:

| Kept                                         | Meaning                                |
| -------------------------------------------- | -------------------------------------- |
| `room:<name\|id\|*>`                         | one room, or all rooms in the account  |
| `account:<id\|user\|*>`, `all:true`          | account scope / everywhere             |
| `sender:` / `from:`                          | MXID substring                         |
| `after:`, `before:`, `date:` (day or `A..B`) | time bounds                            |
| quoted values, `--` terminator               | phrases with spaces; literal free text |

Field tokens are recognized anywhere in the string — `matrix from:adam` and
`from:adam matrix` are the same query. This departs from the TUI, whose
fields must precede the text; an ordering rule nobody remembers is a trap,
and `--` already covers the literal case.

Dropped: `received:` (synonym noise), `limit:` (a terminal concern; the web
pages implicitly), and the TUI's full human-date parsing (`"last Tuesday"`).
Dates are ISO `YYYY-MM-DD`, `MM/DD/YY[YY]`, and the two relatives everyone
actually types: `today`, `yesterday`. Bounds keep TUI semantics: `after:` is
start-of-day, `before:` and `date:` are inclusive end-of-day.

Room and account names resolve against the loaded stores (reusing the `/room`
command's resolver). An unresolvable `room:` becomes a visible chip error, not
a request the server would misinterpret as free text.

Scope defaults follow the point of invocation: opened from a room route, the
scope chip starts at that room; opened elsewhere, it starts at all accounts.

### Submit on Enter; server-side relevance or date ordering

Queries run on Enter, not per keystroke. The endpoint is an offset-cursor BM25
query — cheap, but search-as-you-type buys little for message search (users
know what they are looking for) and costs debounce complexity, request racing,
and result flicker. It is recorded here as a possible follow-up, not scoped.

Results render in the server's relevance order by default.
Newest/oldest request `sort=newest|oldest` from the server, which orders all matches by `origin_ts` before selecting a page.
Changing sort starts again without a cursor and discards the previously loaded pages; subsequent pages use the same query and sort.
The cache and URL identify both the query and sort, so reopening the overlay retains its last ordering and shared date-sorted links query the correct first page.
Generation guards apply to results, errors, and unavailable status so a superseded response cannot overwrite a newer ordering.

Pagination follows the timeline's pattern exactly: a "Load more" button
whenever a cursor remains, plus an `IntersectionObserver` sentinel that clicks
it automatically. jsdom deliberately lacks the observer, so unit tests drive
the button; Playwright covers the sentinel.

### Snippets are built client-side, from plain text only

Each result row shows room title, sender display name, timestamp, an account
tag when the scope spans accounts, and a snippet: a ±60-character window
around the first term hit, ellipsized, with matches wrapped in `<mark>`.
Matching is case-insensitive per term, treating quoted phrases as units.

The snippet renders from the event's plain-text `body`, never
`formatted_body`. Slicing HTML at arbitrary offsets produces broken markup at
best and an injection surface at worst; `EventBody` (which handles formatted
content safely) is a timeline renderer, the wrong shape for a hit list. Media
events fall back to their filename body; redacted events render as deleted.

### Entry points: `/`, `Ctrl-G`, a topbar button, and `/search`

Following ADR 0078's rule — a bare chord needs a modifier twin, because the
composer owns focus — search binds **`/`** (the GitHub/Zulip convention;
`useShortcuts` already withholds bare characters while typing) and
**`mod+g`** with `whileTyping`, reachable from the composer. `Ctrl-G` is the
browser's find-again, which every major browser lets a page intercept and
which loses nothing a message-client user needs; `mod+shift+k` (Firefox
devtools) and the taken `mod+k`/`mod+shift+f` were rejected. Both spellings
enter `KEYS`/`SHORTCUTS`, so the help popup and tooltips stay in sync. A
search button sits in the topbar next to help, for the user who knows no
chords.

The composer gains a `/search [args]` slash command for TUI muscle memory: it
opens the overlay with the args as the query, prepending `room:<current>` when
the args carry no scope token — the same current-room default the TUI applies.

## Consequences

- The web client gains its first global (non-room-scoped) data surface and its
  first URL-addressed modal. The pattern — overlay presence keyed on a query
  param — is available to future features (e.g. a jump-to-room switcher).
- A `SearchStore` joins the global service graph (`services.ts` and the test
  mirror), unlike the per-room stores created inside `RoomPage`.
- `503` renders as "search is not enabled on this server", distinct from
  errors; deployments without the index get a legible state, not a broken box.
- Search results show only matching events, not surrounding conversation; the
  one-click jump into the room _is_ the context view. The TUI's inline
  context-window fetch is explicitly not ported.
- Not in scope: search-as-you-type, server-side snippets/highlighting (a
  server-silo change), searching within results, saved searches, and any
  change to the `q` grammar the server itself parses.
