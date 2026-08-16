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
already exist; `SdkGateway::leave` does not require `RoomState::Joined`
and does not require a local SDK `Room` handle. Sliding Sync can evict an
invited room from `get_room` while the `room_invites` row is still the
inbox source of truth; reject then sends the same
`POST /_matrix/client/v3/rooms/{roomId}/leave` the SDK would. A
successful leave also deletes the invite row (the watcher cannot observe
positive absence when `get_room` stays `None`).

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
same hydration race ADR 0070 hit.

**Every prune needs per-room positive evidence:** if `get_room(id)` returns a
room whose `state() != Invited`, delete the row. A now-joined or left room is
definitely not a pending invite.

Absence from `invited_rooms()` is **not** such evidence, not even when the
list is non-empty. `invited_rooms()` is `rooms_filtered(INVITED)` over the
SDK's in-memory room store — the very same partial knowledge that makes
`get_room` return `None` for a still-pending invite — so a non-empty result
proves only that *some* invites have hydrated, never that the list is
complete. Set-difference against it deletes valid invites whenever the SDK
knows one of three, and re-creating the row later resets `invited_at` and
re-sorts the user's inbox. There is deliberately no store primitive for
"delete every row not in this list."

A withdrawn invite whose room the SDK has forgotten may linger until the
account is deleted (`ON DELETE CASCADE`) *or* the user rejects it. That is
the acceptable direction: a stale row is visible and the user can clear it,
whereas a wrongly-deleted row is silent and unrecoverable. Reject is a
positive signal, but only when the homeserver confirms it — see below.

### Reject must not delete on an unconfirmed leave

`POST .../leave` falls back to a raw client-server `leave` when the SDK has
no local `Room`. A homeserver `M_FORBIDDEN`, `M_NOT_FOUND`, or `M_UNKNOWN`
there usually means "not in that room" (already left, invite withdrawn, or
the server no longer resolves it), but `M_FORBIDDEN` is also what a server
ACL returns, and with no `Room` to corroborate against there is nothing to
tell them apart.
`SdkGateway::leave` therefore reports `LeaveOutcome::{Left, Unconfirmed}`
rather than a bare `Ok(())`. Both answer `200` — the request stands — but
only `Left` may drive the `room_invites` delete. (`Room::leave` can absorb
the same error internally because it follows up with `room_left()` +
`forget()` and converges its own state; the raw fallback has no such
follow-up.)

### The dedup cache is not the source of truth

The watcher's in-memory snapshot cache exists to suppress redundant writes
and `invite.added` frames, so a cache *miss* must never be read as "there is
no row". It carries a `seeded` flag for exactly that: until the cache is
known to mirror the table, a miss means "unknown" and the store gets the
final say. Each sweep re-reads `room_invites` and reconciles the cache
against it, which is what lets the watcher recover both from a failed
startup seed (otherwise pruning stays disabled and every standing invite is
re-broadcast) and from rows deleted out of band by the API (otherwise a
stale matching snapshot suppresses the re-persist for the life of the
process).

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
