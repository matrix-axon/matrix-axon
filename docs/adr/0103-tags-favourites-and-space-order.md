# ADR 0103 — Tags, favourites, and space order

**Status:** Proposed — design only. Tracked in issue #365;
this document is #366. Implementation is #367 (server), #368 (web),
#369 (TUI favs), #370 (TUI spaces).

Supersedes ADR 0038 Phase 2 and the still-open half of ADR 0055 Tier 1
(`tags` / `is_direct` on `RoomDto`; `room_type` already shipped).
Unblocks ADR 0069 M19-W7 and ADR 0079 TUI-M19-3's tag-backed room list.
ADR 0100's "tags are blocked, not deferred" is this work.

## Context

ADR 0068 M19d shipped `PUT`/`DELETE /v1/accounts/{account_id}/rooms/{room_id}/tags/{tag}`
for `m.favourite`, `m.lowpriority`, `m.server_notice`, and `u.`-prefixed custom
tags. Issue #319 recorded that those routes are write-only:

- `RoomDto` has no `tags` field.
- No room-account-data read exists. ADR 0084's `/info` `/pinned` `/space/*`
  `/upgrade` reads are state events; `m.tag` is room **account data**.
- `/v1/ws` has no account-data frame. A favourite set in Element is invisible
  until the next full room-list refresh.
- `set_tag` / `remove_tag` call the homeserver and return. They do not upsert
  the local `account_data` row, so a client that `PUT`s then `GET /v1/rooms`
  can still see no tag.

Both clients already have a local stand-in that does not follow the account:

- Web: `localStorage` `axon.settings.pinnedRooms` (star, `/pin`, Favs filter).
- TUI: `config.toml` `[display] pinned_rooms` (`/pin`, `p` / `Shift-p`,
  `/filter fav`). ADR 0038 Phase 1; Phase 2 was "a future ADR".
- Web space rail: `axon.settings.spaceOrder` — same local-only problem, and
  the picker is one mixed-account sequence (`accountId/roomId` keys).

ADR 0055 already chose Tier 1 fields on `RoomDto` for list-driving signals
(`is_direct`, `room_type`, `tags`) and Tier 2 generic account-data reads as
the escape hatch. `room_type` landed; `tags` and `is_direct` did not. ADR 0084
superseded Tier 2 for **state** clusters and left those two fields "a
separate, still-open gap".

The TUI still ignores `room_type` and has no space tree (issue #65).

## Decision

### Tags and `is_direct` live on `RoomDto`

Issue #319 option 1, ADR 0055 Tier 1. A per-room `GET …/tags` would be N+1 on
the room list.

```text
tags: [{ "name": "m.favourite", "order": 0.25 }, { "name": "u.work" }]
is_direct: bool
```

`Vec<RoomTag>` (`name`, optional `order`) rather than a JSON map, so OpenAPI
can name the shape. Omit empty `tags` (`skip_serializing_if`) so untagged
rooms do not grow the already-large list (#279).

Populate in `Store::list_rooms` from `account_data` (ADR 0016), not new
`room_summaries` columns unless latency says so:

- **tags:** `LEFT JOIN account_data` on `(account_id, room_id, 'm.tag')`,
  parse `content.tags`.
- **is_direct:** load each account's global `m.direct` (one row per account)
  and mark rooms whose id appears in any of that map's arrays, in Rust after
  the SQL.

Older clients ignore unknown fields.

### Write-path upsert and `account_data.changed`

After a successful `set_tag` / `remove_tag`, upsert the room's `m.tag` row so
the next `GET /v1/rooms` is correct without waiting for the sync echo.

On every `persist_account_data` (sync ingest **and** that upsert), publish:

```text
type: "account_data.changed"
payload: { room_id?: string, event_type: string, content: object }
```

`room_id` omitted for global Matrix account data (`m.direct`). Clients that
care about `m.tag` patch `RoomDto.tags`. This is the account-data analogue of
`unread_counts.changed` (ADR 0070), not a raw timeline event.

A tags-only frame would also work; a generic Matrix account-data frame also
carries `m.direct` without a second type. Issue #343's "push resolved
metadata" point applies to **state** events (`m.room.name`) and is not this
ADR.

Do **not** build generic Matrix account-data GET/PUT here. Tags ride
`RoomDto`; space order is not Matrix account data (below). ADR 0055 Tier 2
global stays a later item.

### Space order is an Axon instance preference

Space-rail order is not `m.tag` (that would dump spaces into Element's
favourite **rooms** list) and not Matrix account data (that store is per
Matrix user, so it cannot represent one sequence that interleaves spaces from
several accounts). MSC3230 is the same per-account trap plus lexicographic
midpoint strings. ADR 0048 `device_state` is `account_id`-scoped and
`ON DELETE CASCADE`s with that account — stuffing a global rail there would
vanish when that account is removed.

Axon is one human per process. Add `instance_preferences` (`key TEXT PRIMARY
KEY`, `value JSONB NOT NULL`, `updated_at` via the shared trigger). No
`account_id`. Last-write-wins on the whole value.

- First key, allowlisted: `space_order`.
- Value: `{ "spaces": ["{account_id}/{room_id}", ...] }` — the keys the web
  picker already uses.
- `GET` / `PUT /v1/preferences/{key}`, **not** nested under an account.
  Unknown keys `400`. Size cap 64 KiB, matching device_state values.
- PUT body includes `device_id` so the live frame can echo-suppress.
- After PUT: persist and emit `preferences.changed` `{ key, value, device_id }`.
  Receivers drop frames whose `device_id` is their own (ADR 0048's rule).

This follows every Axon client of **this** instance. It does not go to the
homeserver, so it does not follow the user to Element or to a freshly
installed second Axon. That is the correct durability for a mixed-account
rail: Element has no place to put it.

### Favourites are `m.favourite`

Pin chrome stays (star, `/pin`, TUI `p`). The durable meaning is Matrix
`m.favourite` with `order` in `[0, 1]`. Existing tag-name and order
validation in `crates/axon-sync/src/gateway.rs` is unchanged.

**Migration, once per client:** after the first `GET /v1/rooms` that includes
`tags`, partition local pins by account. If that account already has any
`m.favourite` (Element or another Axon client), discard the local pins
(server wins — ADR 0038 Phase 2). If it has local pins and zero
`m.favourite`, `PUT` each with `order = index / n` (index 0 = top), then
stop writing the local list. Leave the old config key in place so older
builds do not crash.

Space-order migration is the same shape against `instance_preferences`:
server value wins if set; otherwise one-shot upload of `settings.spaceOrder`.

ADR 0069's "do not treat optimistic local state as the source of truth" still
applies. Pending UI is fine until the live frame or refresh lands.

### Web room-list drag-and-drop

The favourite prefix is drag-reorderable, matching the space rail. `m.tag`
`order` is a per-room float, so a mixed-account favourite list still sorts as
**one** sequence. Dragging room A above room B is one `PUT` on A with a
midpoint; B is untouched.

- Drag-and-drop on the row (same Firefox `text/plain` dataTransfer trick as
  `SpaceList.tsx`).
- Alt-↑/↓ on a focused row moves it in the **visible** list.
- Session-only reorder toggle (a shortcut distinct from `KEYS.reorderSpaces`)
  reveals up/down buttons; hidden by default.
- Drop inside the favourite prefix → `PUT m.favourite` with midpoint order
  (pinning an unpinned room if needed).
- Drop on or below the separator → `DELETE` (unpin). The unpinned tail is
  **not** stored; `roomSort` still owns it. Drag among unpinned rooms only is
  a no-op.
- Midpoint: no previous → `next / 2` or `0`; no next → `(prev + 1) / 2`;
  both → `(prev + next) / 2`. If `next - prev` is below ~`1e-10`, rebalance
  visible favourites as `i / (n + 1)`.
- Debounce ~300 ms on keyboard repeat.
- Star / `/pin` stay pin-to-top.
- Filtered lists use visible neighbours. A favourite hidden by the current
  space/filter can sit between the two rooms the user just placed.

Custom order of unpinned rooms is out of scope (it would fight `roomSort`).
The TUI is not a drag target; `p` / `/pin` stay pin-to-top.

### Client sequencing

One silo per PR; server first.

1. **This ADR** (#366).
2. **Server** (#367): `RoomDto` fields, write-path upsert, both live frames,
   `instance_preferences`.
3. **Web** (#368): replace `pinnedRooms` / `spaceOrder`, favourite
   drag-reorder, `is_direct` DM heuristic. Depends on #367.
4. **TUI favs** (#369): replace `pinned_rooms`, deserialize `room_type` but
   do not use it yet. Depends on #367; can land in parallel with #368.
5. **TUI spaces** (#370): issue #65 tree in the room list (not a web-style
   rail), `space/children`, `space_order` GET/PUT, session expand/collapse,
   per-group favourite sort. **After #369, not before** — same silo, and the
   tree must sort with `m.favourite` already in place.

TUI spaces: space roots ordered by `space_order`; children indented;
ungrouped section at the bottom; `/filter fav` still flattens; an account
with no spaces is the PR 4 flat list. Do not lift favourites out of their
space to a global pinned prefix. Out of #65's original scope, still out:
space create, recursive walk, rooms in multiple spaces (first parent wins).

Low-priority and custom tags are present on the DTO and writable via the
existing routes. No new UI in these PRs.

## Consequences

- **Pro:** M19-W7 unblocks. A favourite toggle can show its own state, on
  load and when another client changes it.
- **Pro:** Pins follow the Matrix account (Element included). Space sequence
  follows this Axon instance, which is the only store that can hold a
  mixed-account rail.
- **Pro:** `is_direct` retires the name/alias DM heuristic in both clients
  (ADR 0042 / web `AGENTS.md`) in the same server PR as tags.
- **Con / accepted:** space order does not sync to Element or to a second
  Axon. A global mixed-account sequence cannot.
- **Con / accepted:** write-path upsert of `m.tag` can briefly disagree with
  a racing sync echo; last writer to `account_data` wins, same as today's
  account-data ingest.
- **Con / accepted:** TUI spaces is a tree, web spaces is a filtering rail.
  They share `space_order` and `m.favourite`, not a layout.

## Out of scope

- Issue #343 (`room.metadata_changed` for name/topic/avatar).
- Issue #344 (parallel room-settings writes).
- Generic Matrix account-data or state passthrough.
- Custom / low-priority tag UI; TUI `/favorite` as a `/pin` alias
  (ADR 0079 can add it later).
- Custom order of unpinned rooms.
- TUI space create, recursive hierarchy, multi-parent listing.
