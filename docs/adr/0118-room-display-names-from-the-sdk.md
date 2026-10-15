# ADR 0118 — Room display names from the SDK's calculated name

**Status:** Proposed; tracked in [issue 716](https://github.com/matrix-axon/matrix-axon/issues/716).

## Context

A room with no `m.room.name` and no `m.room.canonical_alias` has no name of its own.
The Matrix spec says how a client should name it anyway ([calculating the display name for a room](https://spec.matrix.org/latest/client-server-api/#calculating-the-display-name-for-a-room)): build it from the room's "heroes", up to five other members, with the joined and invited counts deciding between "Alice", "Alice, Bob" and "Alice, Bob, and 3 others", and "Empty Room (was Alice)" or "Empty Room" when nobody else is left.
Element, Element X and other clients built on matrix-js-sdk or matrix-rust-sdk all follow it, and none of them consult `m.direct` to do so.

Axon does not expose that name.
`RoomDto` carries only `name` and `canonical_alias`, so each client derives its own fallback:

- The web client (`stores/rooms.ts`, `resolveUnnamedTitles`) fetches `GET …/rooms/{room_id}/members` and joins the other members' names, but only for rooms the account's `m.direct` lists (`is_direct`), to keep the per-room fetches from fanning out across the whole list.
  Every other unnamed room renders as its room ID.
- The TUI (`app/rooms.rs`) makes the same `/members` fetch for every unnamed room in the visible window plus a lookahead, with no `is_direct` gate, so it pays one request per unnamed room it scrolls past.

The web client's gate is where the visible bug comes from.
A three-person room joined by invitation is in nobody's `m.direct` from the joining account's side, so it shows `!JdPVlZTXqFtbmvAgXC:bostoncoop.net` in one account and a name in another.
On the production `axona` database 280 of the 444 unnamed rooms on the main account render as a room ID this way.
The other 164 cost the web client one `/members` request each on every fresh session.
The member-joined titles also count bridge bots and the user's own bridge puppet, so a bridged Slack DM is titled "Adam Kessel, Estefany Benitez, Slack bridge bot".

matrix-rust-sdk already computes the spec name.
Axon syncs through `SyncService` (Simplified Sliding Sync), and on every sync response `matrix-sdk-base` recomputes the display name of each updated room (`response_processors::room::display_name::update_for_rooms`), caches it on `RoomInfo`, persists it in the SDK state store, and raises a `RoomInfoNotableUpdateReasons::DISPLAY_NAME` notable update.
It uses the server-supplied heroes from the MSC4186 room response, falls back to locally known members when there are none, and excludes service members listed in `io.element.functional_members`.
`Room::cached_display_name()` and `Room::display_name()` are public upstream API; the vendored `matrix-sdk-base` patch (ADR 0111) does not touch them.

Before deciding, the SDK state stores on production were read offline from copies to check coverage on rooms that have been quiet for a long time:

| Account                      | Joined rooms | With a cached name | Unnamed | Unnamed with a cached name |
| ---------------------------- | ------------ | ------------------ | ------- | -------------------------- |
| `@adam:bostoncoop.net`       | 5,630        | 5,630              | 444     | 444                        |
| `@ajkessel76:pikaviestin.fi` | 4            | 4                  | 1       | 1                          |

Only 6 of those 444 unnamed rooms had SDK member counts from the current membership epoch, so the cached name does not depend on a fresh summary.
Compared against the member rows Axon itself stores, with service members excluded, 439 of the 444 cached names named exactly Axon's other joined or invited members, and the remaining 5 named a hero Axon has no active member row for.
The 23 "Empty Room" variants were all rooms where only the user and service members remain.

## Decision

### 1. The SDK's cached display name is the source of truth

Axon persists what matrix-rust-sdk computes and does not reimplement the naming algorithm.
This is the same choice ADR 0070 made for unread counts: one implementation of the Matrix semantics, shared with Element X, rather than an Axon copy over `room_state` that would have to re-derive heroes, service members, member counts and the empty-room cases and would drift.

The stored value is the SDK's rendered string for every variant (`Named`, `Aliased`, `Calculated`, `EmptyWas`, `Empty`), so `display_name` always means "the name a Matrix client would show".
Rendering `Empty Room` server-side in English is acceptable while Axon is English-only.
A structured kind can be added beside it later without breaking anything.

### 2. Persist it on `room_summaries`, written by the existing SDK reconciler

A new nullable `room_summaries.display_name` column holds the value, so `GET /v1/rooms` stays a single scan of one row per room (ADR 0095).

The ADR 0111 member-count reconciler (`crates/axon-sync/src/member_counts.rs`) already does exactly the traversal this needs.
It subscribes to `room_info_notable_update_receiver()`, reconciles the hinted rooms, and runs a bounded, paced sweep of every summary row to catch rooms whose hints were lost.
Each per-room step now also reads `Room::display_name()` and writes it if it changed.
A second worker would walk the same 5,600 rooms on the same schedule to read the same `RoomInfo`.

The write is a compare-and-set (`display_name IS DISTINCT FROM $new`) and reports applied, unchanged, or missing.
A missing summary row (a room with no stored event yet) uses the reconciler's existing bounded retry queue, and the sweep picks it up later if the retries run out.
One worker runs per account and handles rooms sequentially, so two writes for one room never race; each write reads the SDK's current value at the moment it runs.
The value is capped at 512 characters on a character boundary before it is stored, since hero display names are user-controlled.

`rebuild_room_summaries` deletes and reinserts rows and so drops the column, as it already drops member counts; the next sweep refills it.
No migration backfill is attempted, because the value lives only in the SDK store; the first sweep after upgrade fills every room.

### 3. Expose it additively

- `RoomDto.display_name: string | null` — null until the reconciler has written a value for the room.
  `name` and `canonical_alias` keep their meaning: the raw state values, which room settings read and write.
- A `room_display_name.changed` WebSocket frame carries `{room_id, display_name}` whenever a write applies, so a member renaming themselves updates open clients the way `unread_counts.changed` does.

Both are additions under `/v1/`, so older clients keep working (ADR 0099).

### 4. Clients prefer state, then the calculated name

Clients title a room `name`, then `canonical_alias`, then `display_name`, then the room ID.
Reading the state fields first means a rename or a new alias shows the moment its state event arrives, rather than waiting for the reconciler.
With `display_name` in hand both clients drop the `/members` title fetch, and the web client drops the `is_direct` gate on titles.
The web client keeps its `/members` fetch only for 1:1 DM avatars, which this ADR does not change.

### 5. Server first, one silo per PR

1. This ADR.
2. Server: migration, store, reconciler, `RoomDto` field, frame, OpenAPI.
3. Web client: consume `display_name` and the frame, and drop the title fetch.
4. TUI: the same.

The client PRs follow once the server PR has settled, per the server-first rule in `AGENTS.md`.

## Consequences

- Unnamed rooms get the name every other Matrix client shows, whether or not `m.direct` lists them, and bridged rooms stop listing bridge bots.
- Clients stop issuing a `/members` request per unnamed room to build titles.
- Titles can differ in wording from today's: members are joined with ", ", more than the heroes cover reads "…, and N others", and a member with no display name appears as their user ID localpart.
- A freshly upgraded server returns `display_name: null` until the first sweep reaches a room, so clients still fall back to the room ID during that window.
  On the production account the sweep visits four rooms per tick, so a full pass over 5,600 rooms takes about 25 minutes; hinted rooms are written immediately.
- The SDK's value can lag a membership change by one sync response, and it can name a hero Axon has no member row for.
  Both are the same behavior Element X has.
- Axon now depends on matrix-rust-sdk's naming behavior, including its English strings.
  An SDK upgrade that changes the wording changes Axon's titles; the store and API do not need to change for that.
