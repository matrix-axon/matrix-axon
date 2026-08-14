# ADR 0091 — Invited-room visibility

## Status

Accepted.

## Context

An invited room never appears in `GET /v1/rooms`. That list is derived from
persisted `events` (`Store::list_rooms`); `axon-sync` only persists timeline
and state for rooms the SDK is joined to. Matrix delivers a pending invite as
**stripped state** (`invite_state`) — typically name, avatar, join rules,
encryption, and the inviter's member event — and those events usually have no
`event_id` and no `origin_server_ts`.

That rules out the existing tables. Writing stripped events into `events`
would invent ids, pollute timeline and search, and collide on
`(account_id, event_id)`. Writing them into `room_state` would violate that
table's `event_id`/`origin_ts` NOT NULL contract and poison the joined-room
projection once the invite is accepted. ADR 0037's leave/ban filter also
cannot surface a room that has no event rows at all.

ADR 0068 / 0069 and `docs/client-parity.md` already named this as the largest
remaining Tier-C gap (issue #279): without a durable invite projection, M19b's
`invite` verb is only useful outbound, and no client can show an inbox.
Accept (`POST …/rooms/join`) and reject (`POST …/rooms/{room_id}/leave`)
already exist; `SdkGateway::leave` does not require `RoomState::Joined`.

`join_candidate_invites` (ADR 0040) still auto-joins direct invites from
known contacts so cross-user SAS verification can proceed. This ADR does not
change that. Those invites may appear for one poll tick and then vanish.

## Decision

### Dedicated `room_invites` projection, not `list_rooms`

One row per `(account_id, room_id)` the local user is currently invited to.
Display fields (name, avatar, topic, alias, room type), inviter id / display
name, `is_direct`, `encrypted`, and `invited_at` (first-seen server clock;
never bumped on a later refresh). `updated_at` is trigger-maintained.

The inbox UX is a single "Invites" item, not invited rooms mixed into the
joined list, so `GET /v1/rooms` / `RoomDto` stay joined-only.

### SDK is the source; Axon caches

Same shape as ADR 0070. A per-account `watch_invites` child task (same
cancel/join lifecycle as `watch_unread_counts`) reads
`Client::invited_rooms()` and `Room::invite_details()` / name / avatar /
topic / alias / `is_direct` / `encryption_state`. It does **not** parse raw
stripped JSON or persist fake events.

Subscribe to `room_info_notable_update_receiver` before the startup sweep.
Dedup on the display snapshot so identical captures neither rewrite nor
re-broadcast. A `Lagged` gap is self-healing: the next update or periodic
re-sweep re-derives the current invited set.

### Reconcile absence carefully

Guardrail 5: a gone invite must disappear. Do **not** treat an empty
`invited_rooms()` as "the account has no invites" on startup — that is the
same hydration race ADR 0070 hit. Two complementary prunes:

1. **Positive absence (always safe):** if `get_room(id)` returns a room whose
   `state() != Invited`, delete the row. A now-joined or left room is
   definitely not a pending invite.
2. **List prune (only when `invited_rooms()` is non-empty):** delete rows
   whose room id is not in that list. An empty list is "not loaded yet," not
   "zero invites."

A withdrawn invite whose room the SDK has forgotten may linger until the
account is deleted (`ON DELETE CASCADE`). That is accepted; inventing
absence from an unloaded store is worse.

### Wire

- `GET /v1/invites?account_id=` — cross-account, active accounts only,
  newest `invited_at` first. Empty list is `200`. No new mutation routes.
- `/v1/ws`: `invite.added` (full invite payload) and `invite.removed`
  (`room_id`). The bus is lossy; `GET /v1/invites` is the reconnect source
  of truth.

Client inbox UI is a separate silo (web first). TUI is out of scope here.

## Consequences

- A pending invite is listable after restart without mixing into
  `GET /v1/rooms`.
- Accept/reject stay the existing join/leave verbs.
- Known-contact DM auto-join is unchanged; those invites may flash in the
  inbox and then disappear.
- Stripped events never enter `events` or `room_state`.
