# ADR 0114 — TUI room information from cached metadata

**Status:** Proposed; implementation proceeds through [issue 653](https://github.com/matrix-axon/matrix-axon/issues/653).
This is the TUI consumer of [ADR 0111](0111-cached-progressive-room-metadata.md) for joined rooms, and delivers the joined-room half of [issue 624](https://github.com/matrix-axon/matrix-axon/issues/624).

## Context

`/whereami` opens a room-information popup built only from `RoomDto` and from `m.room.member` events that happen to be in the loaded timeline.
It prints "unavailable (API support needed)" for the alias list, encryption, access, and room type/version, and its member list reflects whatever history is on screen.
[Issue 71](https://github.com/matrix-axon/matrix-axon/issues/71) asked for one `/info` endpoint carrying everything, including a complete member list with roles.

The server went a different way, and most of the data now exists:

| Read                                           | Provides                                                                                                                     | Limits                                                                                                              |
| ---------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `GET …/rooms/{room_id}/metadata` (ADR 0111)    | Eight typed snapshots: aliases, creation, join rules, encryption, power levels, server ACL, history visibility, guest access | Each snapshot carries a status; `unknown` is not "unset". Guest access and server ACL are not in the sync defaults. |
| `GET …/rooms/{room_id}/info` (ADR 0084, 0111)  | `member_counts {joined, invited, observed_at}`                                                                               | Null until observed; `observed_at` is when the pair last changed locally, not upstream freshness.                              |
| `GET …/rooms/{room_id}/upgrade` (ADR 0084)     | Successor room from the tombstone                                                                                            | The predecessor is also in the creation snapshot.                                                                   |
| `GET …/rooms/{room_id}/members`                | User ID, display name, membership, avatar                                                                                    | Reads the lazily loaded member projection, so a large room returns a partial list. Unbounded response.              |

None of these reads has a live frame.
[`docs/room-metadata.md`](../room-metadata.md) asks clients to re-read on reopen, on reconnect, after relevant timeline state events and redactions, and by bounded polling while the panel is visible.

Two of issue 71's fields remain unavailable: a complete member list, and resolved per-member roles.
`/metadata` exposes configured power levels only, and the resolved read drops room-version-12 creator privilege ([issue 324](https://github.com/matrix-axon/matrix-axon/issues/324)).

## Decision

### Read through Axon, off the event loop

The TUI adds client methods for `/metadata`, `/info`, and `/upgrade`, and extends its `MemberDto` with `membership`.
All four reads use the existing read timeout and a byte cap applied before decoding.
The server's snapshot status is mirrored with a catch-all variant, so a status added later degrades to "unrecognized" instead of failing the whole read.

`/whereami` opens the popup immediately from the room summary.
It then spawns the reads and applies their results through the main-loop outcome channel; nothing is awaited from key handling or drawing.
State is held for the one room the popup last showed, with each read independently loading, loaded, or failed.
Holding a single room bounds the cache without an eviction policy, and reopening the same room shows its earlier data while the refresh runs.
At most one fetch is in flight, a result for a room that is no longer shown or from a superseded request is dropped, and a failed refresh keeps the last good data and marks it as possibly stale.

`/metadata` is the source for join rule, history visibility, guest access, and encryption, because it distinguishes unknown from unset.
`/info` is read only for member counts.

### Say what is known, and no more

The popup keeps its identity lines and replaces every "API support needed" line:

- **Aliases:** the canonical alias and "Advertised aliases". The phrase "full alias list" is retired; homeserver-local aliases wait for [issue 621](https://github.com/matrix-axon/matrix-axon/issues/621).
- **Members:** joined and invited counts with the time the pair last changed in Axon's cache (not a freshness signal), or "unknown".
- **Encryption:** algorithm and rotation periods. An unknown snapshot reads "no encryption state cached", never "unencrypted".
- **Access:** join rule with its allow conditions, history visibility, and guest access.
- **Room:** type, version, creator (the create event's sender when the content omits `creator`), creation time, federation, predecessor, and successor.
- **Power levels (configured):** thresholds and non-default users. These are stored values, not permissions, and are labelled as such.
- **Server ACL:** allow and deny counts, shown only when a snapshot exists.

One helper maps `unknown`, `unavailable`, `partial`, `invalid`, `too_large`, and positive redaction evidence to fixed wording.
A `partial` snapshot shows its valid fields and states that some entries were withheld; no Matrix default is substituted for a missing or invalid field.

### Show the member projection as a partial list

The member section is built from `/members`, grouped by membership, with each member's configured power level.
Every loaded member is rendered; the popup already scrolls.
Whenever the authoritative joined count exceeds the loaded list, the heading reads "Members (N loaded of M joined)".
The timeline-derived `known_room_members` list is removed.

Creators are labelled from the creation snapshot.
The TUI does not compute resolved power, and does not call a v12 creator's level a number.

### Search members inside the popup

`/` in the room-information popup opens a filter prompt, the same key the list panes use for search.
Typing narrows the member section to case-insensitive substring matches on display name or Matrix ID and reports how many loaded members match.
The filter is local and covers loaded members only, which the popup states when the room has more members than are loaded.
`Esc` clears an active filter first and closes the popup second.
The key is configurable and listed in `/shortcuts`.

### Refresh while visible

The popup re-reads on open, on a live reconnect, and when a live state event or redaction arrives for the shown room.
While visible it also polls every 30 seconds from the existing tick, with one request in flight, backoff after failures, and cancellation on close.
A background refresh does not move the scroll position or clear an active member filter.
The polling is removed once the server signals required-state-only changes ([issue 343](https://github.com/matrix-axon/matrix-axon/issues/343) and issue 621).

### Land in small, single-silo steps

| Step | Issue                                                             | Change                                                                 |
| ---- | ----------------------------------------------------------------- | ---------------------------------------------------------------------- |
| 1    | [654](https://github.com/matrix-axon/matrix-axon/issues/654)      | Client reads, room-information state, metadata and count rendering, this ADR. Closes issue 71. |
| 2    | [655](https://github.com/matrix-axon/matrix-axon/issues/655)      | Member list from the server projection.                                |
| 3    | [656](https://github.com/matrix-axon/matrix-axon/issues/656)      | Member search in the popup.                                            |
| 4    | [657](https://github.com/matrix-axon/matrix-axon/issues/657)      | Reconnect, state-event, and polling refresh.                           |
| 5    | [658](https://github.com/matrix-axon/matrix-axon/issues/658)      | Smoke assertion against the real stack (smoke silo).                   |
| 6    | [659](https://github.com/matrix-axon/matrix-axon/issues/659)      | API follow-up: complete member list and resolved roles (server first). |

Step 1 depends on PR 645 for `member_counts`.

## Alternatives considered

- **Wait for one complete room-information endpoint, as issue 71 proposed.** The server deliberately split cheap cached reads from bounded acquisition; waiting leaves the popup empty for data that is already available.
- **Keep using `/info` for the four state fields.** It collapses "unknown" and "unset" into null, which would let the TUI claim a room is unencrypted when Axon simply has no cached tuple.
- **Use the resolved `GET …/power_levels` read for member roles.** It reports a v12 creator as `users_default`, so it would label the most powerful member as an ordinary one.
- **Cap the rendered member list.** Cheaper to draw, but it hides people from a list whose purpose is finding them; search addresses length instead.
- **Search by jumping between matches instead of filtering.** It matches the list panes' `n`/`N`, but a filter gives a count and removes the scrolling, which suits a section inside a longer popup.
- **Refresh only on reopen.** Required-state-only updates and asynchronous redaction repair would leave an open popup wrong with no indication.

## Consequences

`/whereami` becomes honest about uncertainty: several lines will read "unknown" for rooms whose state is not hydrated, and guest access and server ACL will usually be unknown until issue 621.
A large room shows a short member list under a large count until issue 659 provides a complete, paginated source; search finds only loaded members until then.
An open popup issues four small local reads every 30 seconds; none contacts a homeserver.
The TUI holds room-information state for one room at a time and drops it when that room leaves the room list.

## Verification

Line-rendering tests cover every snapshot status, null and stale counts, a partial member list against a larger count, a v12 room with an empty `users` map, and a result arriving for a room that is no longer shown.
Paused-clock tests cover polling cadence, backoff, cancellation, and the single in-flight request.
Filter tests cover name and ID matches, no matches, clearing, and scroll clamping after the list shrinks.
Each test is confirmed to fail with its change reverted.
The smoke step then asserts the enriched popup against a real Synapse and Axon stack.
