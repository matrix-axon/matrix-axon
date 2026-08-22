# ADR 0093 — TUI non-blocking startup and bounded per-room work

## Context

Connecting `axon-tui` to a server hosting thousands of rooms left the UI dead
for several minutes: no frame painted, no keystroke handled. Issue #189 traced
it, and the cause was not one slow call.

`run_app` ran five sequential `await`s — accounts, rooms, read markers, the
launch room's timeline, drafts — on the main task **before** the first
`terminal.draw`. Nothing could paint until all five returned, so however long
the load took, the user saw a blank terminal rather than a loading client.

Underneath that, three things scaled with the room count:

1. `request_unnamed_room_titles` swept the whole room list after every refresh
   and spawned one unthrottled `GET /members` per unnamed room. `is_likely_dm`
   is only "no `m.room.name` and no `canonical_alias`", so on a large server
   that is most rooms. Each result then ran an O(n) scan over the room `Vec`
   and, under alpha sort, a full re-sort — quadratic in the room count.
2. A room that resolved to no derivable title was never recorded as such, so
   the sweep re-requested it every cooldown, for the life of the process.
3. Any live event for a room the client did not know returned `RefreshRooms`,
   which re-ran the whole unpaginated `GET /v1/rooms` on the main task and
   re-triggered the fan-out. The WebSocket backlog that accumulated during the
   initial block fed this loop, which is what turned seconds into minutes.

The server-side cost is real but much smaller: issue #85 measures
`GET /v1/rooms` at 1.3 s TTFB for 1,752 rooms. It is out of scope here — a
different silo, and #85 notes pagination alone would not fix its latency.

## Decision

### Startup is a chain of stages, not a run of awaits

`app/bootstrap.rs` owns a four-stage sequence. Each stage's network work is
spawned; its result returns through the main loop's channel and is applied
there. The loop paints and accepts keys from its first iteration.

```text
accounts → rooms → device state (read markers + drafts) → launch timeline
```

The ordering the old code got from writing five awaits in a row is now
enforced structurally: a stage is only spawned from the previous stage's
handler. This matters most for one invariant — **read markers must be applied
before the first `load_selected_timeline`**, or the marker that call fabricates
wins the monotonic merge and permanently discards the real one (ADR 0048,
ADR 0089).

A stage that fails still advances. A server that cannot list accounts may
still have rooms worth showing, and a stage parked on an error would strand
the panel mid-load forever.

**The final timeline load stays an `await` on the main task.** It is one
bounded request for one room's page, it does not grow with the room count, and
it is the same await every room switch already performs. Splitting it would
mean unpicking `load_selected_timeline`'s interleaved fetch-and-apply for no
measurable gain.

### Per-room background work is demand-driven and bounded

The rule media already follows (`clients/tui/AGENTS.md`) now applies to the
room list:

- **Demand-driven.** Titles are fetched for the rooms in the room-list
  viewport plus a small lookahead, topped up from the main loop's tick so
  scrolling and filter changes pull in the rest.
- **Bounded.** `/members` reads hold a permit from a four-slot semaphore, as
  image work holds one from `media_workers`. The pre-existing per-room
  cooldown bounds *repeats of one room*; it never bounded how many rooms were
  in flight at once, which is the property a thousand-room list needs.
- **Negative answers are recorded.** A read that lands and finds nobody to
  name the room after marks the room. A read that *fails* does not — it says
  nothing about the room, so the cooldown lets it retry. Keeping those two
  cases apart is why `MembersOutcome::members` is an `Option` rather than an
  empty `Vec` the caller has to interpret (design guardrail 4).

That marker is kept separate from the cooldown map deliberately: the cooldown
also rate limits the live unknown-sender path, and suppressing *display name*
refreshes because a room has no derivable *title* would be an unrelated
behaviour change.

### Room refreshes coalesce

At most one `GET /v1/rooms` is in flight, with at most one more remembered
behind it. `/refresh`, the refresh shortcut, and the live `RefreshRooms` action
all go through it, so none of them blocks the loop and a backlog of frames
cannot stack up N full fetches. The first `Connected` frame no longer triggers
a re-hydration of the device state that startup is already fetching; later ones
still do, because the bus is lossy (ADR 0048).

## Consequences

- The room list can be empty while startup is still running, so the panel title
  names the stage. An empty list with no label would read as "this account has
  no rooms".
- `refresh_accounts` / `refresh_rooms` / `refresh_read_markers` /
  `refresh_drafts` lose their fetch halves and become `apply_*` functions, so
  one code path applies a result whether it came from startup, a reconnect, or
  a coalesced refresh.
- A room scrolled to for the first time may show its raw id for a moment before
  its derived title arrives. That is the cost of demand-driven fetching, and it
  is the same trade the media path already makes.
- The TUI has no `tracing` dependency, so the diagnostics go where a user can
  read them: the `display.debug` overlay gains per-stage startup timings and
  the room-title cache counts.

## Alternatives considered

**Paginate the room list.** `GET /v1/rooms` takes only `account_id` — there is
no `limit` or cursor in the OpenAPI spec — so the client cannot page today.
Adding one is a server change (`crates/`), and #85 argues it would not fix the
server's latency anyway, since the `GROUP BY` must complete before the ordering
can rank anything.

**Memoize `visible_room_indices`.** It runs once per frame and once per
keystroke. Rejected for now: under the default `RoomFilter::All` it allocates
nothing per room, and a cache would need invalidating from the room list, the
filter, the sort, the account filter, the selection, and the unread map. Six
writers to keep in step, for microseconds, against a stale room list being a
visible correctness bug. The per-room *allocations* inside the filter
predicates were removed instead, which is where the measurable cost was.
