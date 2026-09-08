# ADR 0055 — Room metadata exposure strategy

## Context

The cross-account room list (`GET /v1/rooms`, ADR 0019) returns a curated
`RoomDto` projected from `RoomSummary` (`crates/axon-api/src/dto.rs`,
`crates/axon-store/src/rooms.rs`): `name`, `topic`, `avatar_url`,
`canonical_alias`, `last_activity_ts`, `last_event_id`. That is the only
room-metadata surface clients have.

Several requested features need metadata the server already stores but does not
expose:

- **DM vs. group distinction.** Matrix marks direct chats via the **`m.direct`**
  *global* account-data event — a `user_id → [room_id, …]` map. Axon ingests and
  persists it (`axon-sync` engine; readable via
  `axon-store::state::account_data(account_id, None, "m.direct")`), but nothing
  surfaces a per-room `is_direct` signal.
- **Favorites / pinning via `m.tag`.** ADR 0038 implemented room pinning as
  TUI-local config and explicitly deferred the `m.tag`/`m.favourite` path to a
  "future ADR," noting it "would require new API endpoints." `m.tag` is stored as
  per-room account data today but is not exposed.
- **Room type** (room vs. space — `m.room.create` `type`), **member counts**,
  read markers (`m.fully_read`), and other state/account-data values are likewise
  stored-or-derivable but unexposed.

ADR 0016 established the storage projections (room state and account data as O(1)
point lookups, global account data under a `''` room_id sentinel). The store
layer can already answer all of these reads; the gap is purely the **read API
surface**.

The question this ADR answers: how should Axon expose room metadata generally,
rather than bolting on a one-off field each time a client needs another signal?

## Decision

Adopt a **two-tier model**. Do **not** add a single "all metadata" endpoint:
room metadata is heterogeneous in privacy (room-public state vs. account-private
data), shape (single state values vs. maps vs. derived booleans vs. counts), and
cost (an O(1) name lookup vs. a membership aggregation). One blob would force all
of that into one shape and couple list performance to its most expensive field.

### Tier 1 — curated derived fields on the read DTOs

Extend `RoomSummary` → `RoomDto` (the existing ADR 0016 projection) with a small,
bounded set of **server-computed** signals that the room *list* needs to
decorate, sort, or filter cheaply, and that require server-side derivation or are
universally useful:

- `is_direct: bool` — whether `room_id` appears in this account's `m.direct`
  global account data. Unlike `room_type`/`tags`, this one *could* be computed
  client-side from a single Tier 2 global-account-data read (`m.direct` is one
  map, fetched once, then joined against the room list locally). It's included
  in Tier 1 for consistency with the other two curated fields and so every
  client isn't obligated to reimplement the same join — not because the server
  has no other way to expose it.
- `room_type: Option<String>` — `m.room.create` `type` (e.g. `m.space`), so
  clients can separate spaces from rooms.
- `tags: Vec<RoomTag>` — the room's `m.tag` entries (`tag` name + optional
  `order`), unblocking ADR 0038 Phase 2.

`room_type` and `tags` are the stronger case for Tier 1: both are **per-room**
state/account-data, so a Tier-2-only design would need one API call per room to
decorate a list of N rooms (Tier 2 has no batch-read endpoint) — an N+1 cost
that scales with list size. That per-room fan-out, not "the server can't
expose it," is the actual performance argument for elevating them.

Member/unread counts are **explicitly out of scope for Tier 1's list projection**
because they are the expensive case; if needed they belong on the per-room detail
read below (or a separately cached summary), so list latency does not scale with
the priciest field.

Tier 1 is opinionated and curated: it is where the product decides which signals
are first-class enough to drive list UX. New first-class signals are added here
deliberately, with review, not reflexively.

### Tier 2 — generic account-data and state reads (the escape hatch)

Mirror the Matrix client-server API so any other metadata is reachable without a
bespoke DTO field, keeping the list lean and avoiding DTO churn as needs grow:

- `GET /v1/accounts/{account_id}/account_data/{type}` — global account data
  (e.g. `m.direct`, `m.ignored_user_list`).
- `GET /v1/accounts/{account_id}/rooms/{room_id}/account_data/{type}` — per-room
  account data (e.g. `m.tag`, `m.fully_read`).
- `GET /v1/accounts/{account_id}/rooms/{room_id}/state/{type}[/{state_key}]` —
  current room state (e.g. `m.room.power_levels`, `m.room.join_rules`).

These return the stored `content` JSON as-is. They are the completeness layer;
interpretation of less-common types is the client's responsibility. This is the
read half of the previously proposed generic passthrough endpoint
([issue #130](https://github.com/matrix-axon/matrix-axon/issues/130)); writes
remain out of scope here (see ADR 0038 Phase 2 / the mutations gateway, ADR
0021).

### The dividing line

- **Curated projection (Tier 1)** vs. **raw passthrough (Tier 2)** — Tier 1 is a
  short, reviewed list of product-blessed, list-driving signals; Tier 2 is the
  complete, generic, Matrix-shaped surface for everything else.
- **Room-public state** vs. **account-private data** — both tiers respect this:
  account-data reads must never expose another account's private data. ADR 0029's
  bearer tokens are instance-global, not account-scoped, so Tier 2 routes cannot
  rely on the bearer middleware alone for that isolation; if they keep
  `account_id` in the path, they need an explicit account-ownership check (or a
  different route shape) in the follow-on API design.

A field graduates from Tier 2 to Tier 1 only when it earns first-class list UX
(as `is_direct` and `tags` do now).

### Optional: per-room detail read

A `GET /v1/accounts/{account_id}/rooms/{room_id}` returning a richer room object
(summary + derived fields + counts) is a natural home for fields too expensive
for the list (member counts) that are wanted on room-open. Specified as a
follow-on, not required for `is_direct`.

### Alternatives considered

- **Single "all metadata" endpoint.** Rejected: forces heterogeneous
  privacy/shape/cost into one response and couples list latency to the most
  expensive field. The two-tier split gives the same reach without those
  drawbacks.
- **One-off `is_direct` field only.** Solves DMs but repeats the ADR-0038
  problem: the next metadata need (tags, room type, …) forces another bespoke
  round. Establishing the policy now amortizes that.
- **Client computes everything from raw events.** Already rejected by ADR 0016,
  which exists precisely so reads are point lookups, not folds over history.

## Consequences

- `is_direct` (and `room_type`, `tags`) become available on the room list,
  letting clients distinguish DMs, separate spaces, and honour favourites. The
  TUI can then show a DM marker and migrate pinning to `m.tag` (ADR 0038 Phase
  2).
- The generic Tier 2 reads cover present and future metadata needs without
  further DTO changes; clients parse uncommon types themselves.
- `RoomSummary`/`RoomDto` grow by a bounded, reviewed set of fields. Older
  clients ignore unknown fields (serde default), so the additions are backward
  compatible.
- Tier 2 routes that expose account-private data introduce a new API requirement:
  because ADR 0029 bearer tokens authenticate only at the instance level, a
  route that accepts `account_id` cannot trust that path value on bearer auth
  alone. The follow-on API work must add an explicit account-selection /
  ownership check (or change the route shape) before the keyed store query
  (`account_data(account_id, room_id, type)`) is safe to use for isolation.
- Writes (e.g. setting `m.tag` from the TUI) remain unaddressed here; they need
  the mutations gateway / homeserver round-trip and belong to ADR 0038 Phase 2.
  Tag **writes** later landed as ADR 0068 M19d; the read surface, live fan-out,
  and client migration are ADR 0103 (issue #365).
- This ADR supersedes the "future ADR" placeholder in ADR 0038 Phase 2 for the
  **read** side of `m.tag` exposure. Implementation of that read (`tags` and
  `is_direct` on `RoomDto`) is ADR 0103; `room_type` shipped with the room
  list independently.

### Suggested implementation sequence

1. **Store/API (Tier 1):** add `is_direct` (and `room_type`, `tags`) to
   `RoomSummary` → `RoomDto`, computing `is_direct` from `m.direct`.
   `room_type` has shipped; `is_direct` and `tags` are ADR 0103 / issue #367.
2. **API (Tier 2):** the generic account-data / state read endpoints.
   ADR 0084 superseded this for four state clusters; remaining global
   account-data GET/PUT is not part of ADR 0103.
3. **TUI:** consume `is_direct` for a room-list DM indicator; later, migrate
   pinning to `m.tag` via the new reads. Both are ADR 0103 / issues #369 and
   #370.

Each is its own PR/silo; Tier 1 unblocks the TUI DM indicator on its own.
