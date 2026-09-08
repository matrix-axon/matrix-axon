# ADR 0038 — TUI room pinning

## Context

The TUI room list is sorted solely by recent activity (`Reverse(last_activity_ts)`).
Users with many rooms have no way to keep important rooms permanently accessible at
the top — a room that hasn't received a message recently sinks down the list.

The Matrix spec provides a standard mechanism for this: per-room account-data events
of type `m.tag`, with the predefined tag `m.favourite` and an optional `order` float
field indicating sort position within the tag group. Clients that implement `m.tag`
honor `m.favourite` by floating those rooms to the top, and the state syncs across
all Matrix clients via the homeserver.

The Axon backend currently:
- **Receives** `m.tag` events from the sync pipeline and persists them in `account_data`
- **Does not** expose any API endpoint to read or write room tags
- **Does not** have a write path to push account-data mutations back to the homeserver

Implementing full `m.tag` support would therefore require new API endpoints, a
gateway write path, and homeserver round-trips for every pin/unpin action. That is
out of scope for this feature iteration.

Alternatively, this might be an ideal use case for the proposed [generic Axon API
passthrough endpoint](https://github.com/matrix-axon/matrix-axon/issues/130). This
feature doesn't have any "value added" from the gateway, and it should be simple
te send and retrieve `m.tag` from the upstream homeserver.

## Decision

### Phase 1 — local config storage, TUI-only

Implement room pinning as a pure TUI change. Pinned room state is stored in
`~/.config/axon-tui/config.toml` under `[display]`, serializing each pinned room as
`"account_id:room_id"`. Pinned state is written immediately on every pin/unpin (not
deferred to `/saveconfig`).

**Runtime model**

- `App` gains `pinned_rooms: Vec<RoomKey>` (ordered, index 0 = most recently pinned).
  Populated from config on startup.
- A room is pinned if its `RoomKey` appears in this vec.

**Sort logic** (`app/rooms.rs` — `apply_room_refresh`)

Rooms are partitioned into pinned / unpinned before sorting:
1. Pinned rooms sorted by position in `pinned_rooms` (most recently pinned first).
2. Unpinned rooms sorted by `Reverse(last_activity_ts)` as before.
3. Concatenated: pinned first, then unpinned.

Re-pinning an already-pinned room removes its existing entry and prepends it —
moving it to the top of the pinned section.

**Separator**

A dim `─` separator `ListItem` is injected between the pinned and unpinned sections
in the room list renderer (`ui.rs`) when both sections are non-empty in the current
viewport.

**Slash commands** (`command.rs`, `app/rooms.rs`)

- `/pin [room]` — pin the given room (or currently selected room if no argument).
  If already pinned, moves to top of pinned list.
- `/unpin [room]` — unpin the given room (or currently selected room).
  No-op with a status message if the room is not pinned.

Both commands use the existing `resolve_room_target()` resolution (supports room
number, alias, name, partial match).

**Keyboard shortcuts** (`config.rs`, `keymap.rs`)

Two new configurable bindings, active only when room-list focus is held:
- `pin_room` (default: `p`) — pin or re-pin to top
- `unpin_room` (default: `shift-p`) — unpin

`popup_shortcuts_lines` in `ui.rs` is updated to list both bindings in the room-list
section, following the existing convention.

### Phase 2 — migrate to `m.tag` / `m.favourite`

**Done as design in ADR 0103** (issue #365 / #366). Tag **writes** already
shipped with ADR 0068 M19d; the missing read surface, live fan-out, and
client migration of local `pinned_rooms` / `pinnedRooms` are #367–#369.
Until those land, pinned rooms still do not sync across Matrix clients.

### Not option: server-only `m.tag` from the start

Deferring the feature entirely until the backend write path exists would block a
useful TUI improvement on a large infrastructure change. Local storage is
non-destructive: the `pinned_rooms` config key can coexist with a future `m.favourite`
sync, and the merge strategy in Phase 2 is straightforward.

## Consequences

- Pinned rooms appear at the top of the TUI room list, separated from unpinned rooms
  by a thin horizontal line.
- Pinned state persists across TUI restarts; it does not sync to other Matrix clients
  until ADR 0103's client PRs land (issues #368 / #369).
- Users with existing `m.favourite` tags set from other clients do not automatically
  see those rooms pinned in the TUI until those PRs land.
- The `[display]` section of `config.toml` gains a `pinned_rooms` array; older TUI
  versions that do not know this key will ignore it gracefully via serde's
  `deny_unknown_fields = false` default.
- No backend changes in Phase 1.
