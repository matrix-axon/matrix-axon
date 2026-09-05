# ADR 0084 — Typed read endpoints for the remaining room-state gaps

## Context

Axon deliberately shipped MVP with spaces as first-class API resources out of
scope (PRD, tech-spec §7): `m.space.child`/`m.space.parent` land in
`room_state` like any other state tuple, but nothing resolves them into a
hierarchy — a client gets a room's own state, never a space's children or a
room's parent spaces.

Auditing `room_state` for other state types with the same shape — ingested and
resolvable via `axon-store::Store::room_state`/`room_state_of_type`
(`crates/axon-store/src/state.rs`), but never exposed as a typed read — turned
up three more (issue #404):

- **Pinned messages** (`m.room.pinned_events`): resolvable, and ADR 0074
  already treats pin/unpin as notice-worthy in the timeline, but there is no
  way to read the *current* pinned set.
- **Room info** (`m.room.join_rules`, `m.room.history_visibility`,
  `m.room.guest_access`, `m.room.encryption`): four small singleton reads
  answering "what kind of room is this," none reachable today.
- **Tombstone / upgrade chain** (`m.room.tombstone` successor,
  `m.room.create.predecessor`): the store already reads `m.room.tombstone`
  internally to hide upgraded rooms from `list_rooms`
  (`crates/axon-store/src/rooms.rs`), but never tells the client *why* a room
  disappeared or where it went.

`axon-api` has one precedent for this shape — `GET
.../rooms/{room_id}/members` (`room_state_of_type` → typed DTO → optional
enrichment) — and nothing has connected the remaining state types through it.

This ADR also revises a prior decision. **ADR 0055** ("Room metadata exposure
strategy") proposed a Tier-2 generic passthrough — `GET
.../rooms/{room_id}/state/{type}` — as the intended home for exactly these
join-rules/history-visibility/guest-access/encryption reads, among others. That
passthrough was never built (no `state/{type}` or `account_data/{type}` route
exists in `axon-api` today). Building it now, as originally scoped, would push
Matrix-spec parsing back into every client, lose the typed OpenAPI contract,
and — for spaces and pinned messages specifically — doesn't even fit: spaces
are one-to-many with ordering/suggested flags, tombstone is a 1:1 pointer,
pinned events need a join against `events` for content. One generic shape
doesn't serve any of these cleanly.

## Decision

Supersede ADR 0055's Tier-2 plan for these four state clusters. Extend the
existing typed-read pattern instead: `room_state`/`room_state_of_type` → a
purpose-built DTO → enrichment where needed, one dedicated (small) query per
cluster rather than a generic pass-through. (ADR 0055's Tier-1 fields —
`room_type` shipped; `is_direct` / `tags` are ADR 0103 / issue #365 and
out of scope here.)

Four read surfaces, all under `/v1/accounts/{account_id}/rooms/{room_id}/`:

1. **`space/children`** and **`space/parents`** — backed by
   `m.space.child`/`m.space.parent`, enriched with each referenced room's
   cached name/avatar/`room_type` (from its own `m.room.create`). Children are
   ordered per MSC1772 (`order` string ascending, absent sorts last, then
   `origin_ts`, then `room_id`) — this needs a bespoke query, not a reuse of
   `room_state_of_type`'s `state_key`-ordered read. Unlike a room's own state,
   the number of distinct `m.space.child`/`m.space.parent` state keys isn't
   bounded by anything Axon controls (a remote room over federation could hold
   an arbitrary number), so both reads are capped at `SPACE_HIERARCHY_CAP` rows
   — applied after the MSC1772 sort for children, so a truncated result still
   keeps the leading/highest-priority entries.
2. **`pinned`** — resolves `m.room.pinned_events`, then joins the referenced
   event ids against the timeline projection for real content (same
   redaction/edit/reaction resolution as every other timeline read), returned
   as `Vec<EventDto>` — no new DTO needed. The pinned id list is likewise
   capped (reusing `events.rs`'s existing `RELATION_READ_CAP`) before it drives
   the join, for the same reason.
3. **`info`** — bundles `join_rules`, `history_visibility`, `guest_access`,
   `encryption` as one read, each an independent singleton `room_state`
   lookup.
4. **`upgrade`** — `tombstoned_to` (from `m.room.tombstone`) /
   `upgraded_from` (from `m.room.create.predecessor`). Deliberately **not**
   folded into `RoomDto`: `list_rooms` already excludes tombstoned rooms from
   `/v1/rooms` (ADR 0037-adjacent behavior in `rooms.rs`) so a `RoomDto` field
   there would be unreachable for the one room it matters for. A dedicated
   by-id read works for a client that already holds the old room's id (from
   local history) and wants to know where it went.

`info` and `upgrade` share a small internal helper — a single-value
`room_state` resolve-and-unwrap — since both are pure singleton reads;
`pinned` and the two `space/*` reads are each a genuinely different query
shape and get their own store methods.

### Scope and sequencing

Per issue #404 the natural shape is four independent PRs (no shared code
between them beyond the singleton-state helper, no ordering dependency). This
implementation lands all four in **one combined PR** instead, by explicit
developer choice — total surface is modest (~1,000 lines including tests) and
review cost is judged lower than four-PR overhead for this batch. Scope is
`axon-store` + `axon-api` (+ OpenAPI regen): `room_state`/`room_state_of_type`
already resolve every state type involved, but `space_children`/`space_parents`
(bespoke MSC1772 ordering, plus a row cap) and `pinned_events` (the join
against `events`, plus the same cap) are new store-layer queries, not reuses of
an existing read — only `info` and `upgrade` are pure `axon-api` additions on
top of the existing singleton `room_state` lookup. No migration either way.
Client consumption (TUI/web) is out of scope here, as usual for a server-side
read addition — separate follow-up work per client once the endpoints exist.

## Consequences

- Four new typed, documented read endpoints; `openapi/openapi.json` grows by
  their paths and DTOs.
- ADR 0055's Tier-2 generic state/account-data passthrough is no longer the
  intended path for `join_rules`/`history_visibility`/`guest_access`/
  `encryption`; a future need for genuinely arbitrary state types can still
  revisit Tier 2, but these four are now covered by typed reads instead.
- No schema migration, no change to ingestion — this is a read-projection
  change only, like ADR 0074.
- Landing all four together in one PR trades some of the "each PR reviewable
  in isolation" benefit the issue called for, for lower combined overhead;
  future room-state gaps of this shape should default back to one-PR-per-type
  unless there's a similar reason to batch.
