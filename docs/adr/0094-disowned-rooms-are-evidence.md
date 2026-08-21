# ADR 0094 — A disowned room is evidence

## Status

Accepted. Amends ADR 0091.

## Context

ADR 0091 made a pending invite durable and deliberately refused to prune one
without **positive per-room evidence**: a `get_room(id)` whose `state()` is no
longer `Invited`. Absence from `invited_rooms()` is not evidence, and there is
no store primitive for "delete every row not in this list." That guardrail is
right and this ADR does not weaken it.

The ADR named one residual risk and judged it acceptable:

> A withdrawn invite whose room the SDK has forgotten may linger until the
> account is deleted (`ON DELETE CASCADE`) _or_ the user rejects it.

Production found the case where **neither escape works**. A bridge re-created a
user's chat history as room-version-12 portals, then those rooms were purged
from the homeserver. 200 rows were left behind, and:

- The homeserver reports no pending invites at all — `GET /_matrix/client/v3/sync`
  returns an empty `rooms.invite`, and the rooms are absent from every Synapse
  table (`rooms`, `local_current_membership`, `sliding_sync_membership_snapshots`).
- Sync can therefore never retract them. A purge emits no `m.room.member` leave;
  the server will never mention a room it has deleted.
- ADR 0091's positive-evidence check consults the SDK's own state store, which
  is the thing that is stale. `get_room` keeps answering `Invited` forever.
- Accept fails: `404 M_UNKNOWN`, `Can't join remote room because no servers that
  are in the room have been provided.`
- Reject fails too: `404 M_UNKNOWN`, `Not a known room`.

That last one is the load-bearing surprise. ADR 0091 exempted the `Room::leave`
path from the `Unconfirmed` treatment, reasoning that the SDK "can absorb the
same error internally because it follows up with `room_left()` + `forget()` and
converges its own state." matrix-sdk decides that per error; against a disowned
room it logs `ignore_error=false should_forget=true` and returns `Err` **before**
forgetting anything. So the room stays `Invited` in the SDK, the row stays in
`room_invites`, and the invite is unclearable by any route the API exposes.

## Decision

### A `404` that denies the room is positive per-room evidence

A homeserver answering `404` with `M_NOT_FOUND` or `M_UNKNOWN` to a request that
names exactly one room is that server stating it cannot resolve that room. That
is per-room, positive, and server-sourced — precisely the shape of evidence ADR
0091 demands. It was simply never enumerated as a source, because ADR 0091 only
looked for evidence in the SDK's room state.

`SdkGateway::leave` reports a third outcome, `LeaveOutcome::Gone`, for it. Both
the `Room::leave` path and the no-local-`Room` fallback classify; ADR 0091's
carve-out for the former is withdrawn, since its premise does not hold.

### Membership state does not enter into it

`LeaveOutcome::Gone` is reported whatever state the room is in locally, joined
included. You cannot be joined to a room the homeserver cannot resolve, so `200`
is the honest answer and dropping the room from the SDK's view is convergence,
not loss.

The joined case is also the more forgiving of the two if a `404` were somehow
spurious: Axon's own `events`/`room_state` projections are untouched either way,
the dead-room set is only ever consulted by the invite watcher, and sync
re-delivers a joined room the server does still have. Only the invite path acts
destructively on this evidence, which is why the pair below is narrow.

### The evidence is the `(status, errcode)` pair, not the errcode

`M_UNKNOWN` is Matrix's catch-all and arrives on plenty of answers that say
nothing about whether a room exists. Keying the delete on the errcode alone
would destroy a live invite on an unlucky upstream hiccup. Only `404` +
`M_NOT_FOUND`/`M_UNKNOWN` counts.

`M_FORBIDDEN` is excluded at every status. ADR 0091's reasoning stands: with no
local `Room` to corroborate against, a server ACL and "not in that room" are
indistinguishable, so it remains `Unconfirmed` and keeps the row.

### Accept is evidence too, not just reject

A join that comes back with the same `(404, M_NOT_FOUND | M_UNKNOWN)` learns the
same fact. It records it rather than only surfacing an error, so a user who
tries Accept first is not left with a row they now cannot clear. The join route
takes no `Store`, so the row is dropped by the invite watcher's next sweep,
which is also what emits `invite.removed` for it.

### Both places that resurrect the row must be told

Deleting the `room_invites` row is not sufficient. The watcher re-derives from
`Client::invited_rooms()`, so the row returns on the next sweep, and the SDK's
persisted state brings it back after a restart. On `Gone` the gateway therefore:

- records `(account_id, room_id)` in a set on `ClientManager` — already shared by
  the sync supervisor and the gateway — which `capture_invite` and
  `sweep_invites` both *read*. Neither drains it: matrix-sdk exposes no way to
  evict a room from its in-memory list, so it must keep answering for as long as
  that client keeps reporting the room as `Invited`;
- calls `StateStore::remove_room`, so the room is gone from the SDK's view on
  the next boot and the set does not need to persist.

The set therefore lives exactly as long as the client it shadows, not as long as
the process: `ClientManager::evict` drops an account's entries along with its
client, and the rebuilt client re-derives from the state store the room has
already been removed from.

Both are best-effort. A failure costs a stale row, never a wrong deletion —
the same direction ADR 0091 chose.

## Consequences

- A dead invite is clearable. Accept or Reject on a disowned room now drops the
  row and stops it returning.
- Reject on such a room answers `200`, not `502`. The request stands: there is
  nothing left to leave.
- `M_FORBIDDEN` behaviour is unchanged, so an ACL still cannot silently destroy
  a pending invite.
- The dead-room set is bounded by live accounts rather than by disowned rooms
  accumulated over uptime, since `evict` clears an account's entries. Unlike the
  gateway's power-level locks, it is not an accepted leak.
- Rows stranded before this change are cleared by one reject pass after deploy;
  no migration or manual SQL is involved, and `DELETE FROM room_invites` alone
  would not have worked, since the watcher re-derives from the SDK.
