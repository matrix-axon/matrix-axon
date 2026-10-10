# ADR 0115 — TUI room unread from the server's notification counts

**Status:** Proposed; implemented in [PR 676](https://github.com/matrix-axon/matrix-axon/pull/676), closing [issue 673](https://github.com/matrix-axon/matrix-axon/issues/673) and [issue 674](https://github.com/matrix-axon/matrix-axon/issues/674).
This is the TUI consumer of [ADR 0070](0070-server-derived-unread-counts.md); the web client already reads these counts.

## Context

The web client's room badges come from one source: the server's `notification_count`, seeded from `RoomDto` and updated by `unread_counts.changed` frames.
The TUI derives its room badges independently, from three places:

1. a local counter incremented for every live event `should_show_event` renders in an unselected room;
2. `reconcile_unread_with_markers`, which compares each room's read marker against `room.last_activity_ts` and the newest loaded event;
3. `clear_unread_if_read`, which uses the same timestamps to clear a badge when a sibling device moves a marker.

Issue 673 reported that this over-counts joins, edits, and reactions.
PR 676 narrowed the live counter and the loaded-event timestamp to real messages, but production showed the remaining gap: `room_summaries.last_activity_ts` falls back to the latest event of any type when a room has no content-bearing event, so a message-less room whose only event is a join looks newer than its marker.
On the production `axona` database 333 of 5,605 visible rooms are in that state, and all of them have a server count of 0.
Patching the timestamp comparison again would leave two derivations of one fact, which is how the TUI and the web came to disagree.

## Decision

### The server count is the room badge

`rooms.unread` keeps its shape (room key to a count) but has exactly two writers:

- **Room-list load and refresh** set each room's entry from `RoomDto.notification_count`; zero removes it.
- **`unread_counts.changed` frames** set or remove the named room's entry.
  A frame for a room the TUI does not know requests a room refresh, as the web does.

The `(n)` badge, the unread filter, and the space roll-ups keep reading `rooms.unread` and need no change.
`RoomDto` gains `notification_count` and `highlight_count`, both defaulting to 0 so an older server degrades to "nothing unread".

### The selected room stays clear

A room on screen counts as read.
A count above zero for the selected room, from a load or a frame, is ignored; the receipt the TUI sends for the shown message brings the server value to zero.
This matches the existing rule that opening a room clears its badge.

### Sibling-device reads clear immediately

A `read_markers` entry from another device that moves the room's marker forward clears that badge without waiting for the server count to follow, which is what the web does.
A stale or repeated marker, such as one from a device that was offline, clears nothing.
The echo of this device's own writes is already suppressed upstream, and the marker is no longer compared with any timestamp.

### What is removed

- the live per-event room counter;
- `reconcile_unread_with_markers`, `clear_unread_if_read`, `latest_known_activity_ts`, and `loaded_content_activity_ts`.

### What stays

- **Thread unread.** It is derived from live thread events, has its own picker and previews, and the server publishes no per-thread count.
  `EventDto::counts_as_unread` is kept for it (PR 676), so a reaction, join, or edit in a thread still does not badge.
- **Read-marker and receipt writes.** They are what produce the server's counts.
- **Rendering.** `should_show_event` still decides what is displayed.

## Consequences

- **The TUI matches the web by construction.** Two clients can no longer disagree about whether a room is unread, short of one of them lagging a frame.
- **The number can differ from today's.** The server counts push-rule notifications, so a muted room or a message that does not notify no longer adds to the badge.
- **The TUI inherits the server's known counter bugs.** Under-reporting from a stale `thread_id=main` receipt ([issue 507](https://github.com/matrix-axon/matrix-axon/issues/507)), a redacted event pinning a badge, arrival-order inversion ([issue 505](https://github.com/matrix-axon/matrix-axon/issues/505)), and rejoin catch-up ([issue 625](https://github.com/matrix-axon/matrix-axon/issues/625)) now show in the TUI exactly as they show in the web.
  Fixing them is server work and benefits both clients.
- **Highlights are carried but not styled.** `highlight_count` is parsed and stored; distinct mention styling is left to a separate issue.
- **No new server surface.** The fields and the frame already ship; this ADR adds none, and no `last_activity_is_content` field is exposed.

## Alternatives considered

- **Expose `last_activity_is_content` and ignore the timestamp when it is false.** It fixes the message-less-room case but adds a permanent API field whose only consumer disappears once the TUI reads the counts, and it keeps two derivations alive.
- **Keep the marker reconcile and gate it on `notification_count == 0`.** It is the migration in disguise: it leaves the live counter and the reconcile in place while making the server authoritative for the same decision.
- **A server-side per-thread count.** It would let thread unread follow the same rule, but it is a server change with its own design; it is out of scope here.

## Verification

- Room-list load seeds badges from `notification_count`, and a refresh replaces them, including clearing a room whose count fell to zero.
- An `unread_counts.changed` frame sets and clears a badge; a frame for an unknown room requests a refresh; a frame for the selected room does not badge it.
- A live join, reaction, edit, or message in an unselected room changes nothing by itself.
- A sibling-device read marker clears the room's badge.
- Thread unread behaves as before.
