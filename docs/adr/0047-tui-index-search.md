# ADR 0047: TUI indexed search

## Status

Proposed

## Context

PR 180 adds `GET /v1/search`, a bearer-gated HTTP endpoint that queries the
Tantivy index and returns ranked, hydrated `EventDto` hits. The TUI needs a
keyboard-first way to use this endpoint without making users remember a
shell-style flag grammar.

The existing TUI already has local in-pane search for visible accounts, rooms,
and messages. Indexed search is different: it queries Axon's full corpus,
supports server-side filters, returns relevance-ranked hits, and needs enough
timeline context for users to decide whether a hit is useful.

## Decision

Add a `/search` slash command backed by `GET /v1/search`.

`/search <query>` is the fast path and searches the currently selected room by
default. Bare `/search` opens a guided search form. The command also accepts
compact `field:value` filters before the remaining query text:

- `account:<target>` restricts to one active account.
- `account:*` searches across all accounts.
- `room:<target>` restricts to one visible room.
- `room:*` searches all rooms in the selected/current account.
- `sender:<mxid>` restricts to one sender.
- `from:<mxid>` is a synonym for `sender:`.
- `all:true` is retained as an alias for `account:*`.
- `limit:<n>` controls page size, capped by the server.
- `date:` and `received:` restrict to a day or inclusive date range.
- `before:` is inclusive through the end of the given day.
- `after:` is inclusive from the start of the given day.
- `to:` is a synonym for `before:`.

Date filters use local calendar-day boundaries. Two-digit years mean
2000-2099. Ranges use `date:start-end`; omitting `start` behaves like
`before:end`, and omitting `end` behaves like `after:start`. Multi-word human
date values such as `"last week"`, `"last year"`, `"this month"`, and
`"last Tuesday"` must be quoted.

Search results render in a dedicated overlay rather than the status line. The
overlay lists hits with room, sender, timestamp, and snippet.
The TUI requests `sort=newest` by default, and the results sort shortcut toggles `newest` and `oldest` on the server.
Each sort change restarts pagination without a cursor, so time ordering covers all matches rather than only loaded relevance-ranked pages.
Ungrouped results retain the server's order, including equal-timestamp ties; room grouping remains a local presentation of the loaded results.
A generation counter rejects superseded initial/page responses, including switching back to the same query and sort while an older response remains in flight. The
selected result lazily loads a small timeline context window around the hit.
`Enter` jumps to the result in the room timeline; reply/thread shortcuts first
jump to the result and then use the TUI's normal selected-message action.

## Consequences

The command remains easy for simple searches while exposing the API filters to
power users. The modal gives a discoverable path for users who do not remember
field names.

Context remains a TUI responsibility because PR 180 returns the matching
hydrated event and score, not neighboring events. The TUI builds context through
timeline reads around the hit timestamp.

This work is stacked on PR 180 and should be rebased onto `main` after PR 180
lands. It intentionally does not depend on unrelated open TUI PRs; if they land
first, this branch should be rebased and conflicts resolved against the updated
TUI surface.
