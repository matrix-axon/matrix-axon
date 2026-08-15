# ADR 0090 — Rooms that can no longer clear their own unread count

## Context

ADR 0070 derives each room's unread count from matrix-sdk's read-receipt state
and caches it in `room_unread_counts`, and ADR 0089 fixed the read receipt so
that reading a bridged room actually clears it. Live testing then surfaced rooms
where *neither* mechanism can ever produce a correct value, because the room has
stopped being a thing the homeserver will talk about.

On the reporting node, two rooms had carried a badge nothing could clear for six
days. Synapse had **no row for either in its `rooms` table**, let alone events —
they had been purged upstream, most likely as deleted bridge portals. Axon's own
`room_state` still reported `membership = join`. The failure is entirely silent
by construction:

- Sync stops mentioning a purged room, so matrix-sdk never recomputes its
  counts. The cached row freezes at whatever it last held — in this case written
  4 ms after the room's final event was ingested.
- No read receipt can correct it. Synapse answers
  `404 M_UNKNOWN: Could not find event $… in room !…`, and the client never sees
  that failure (the route is fire-and-forget by ADR 0067; it now at least logs).
- ADR 0070's prune cannot reach it: that prune drops rooms missing from
  `client.joined_rooms()`, and nothing ever contradicted the SDK's local belief
  that the account is joined.

A third stuck room on the same node was a different shape with the same effect:
an **upgraded** room, tombstoned by its bridge, frozen at 2 since the upgrade.
`Store::list_rooms` already excludes tombstoned rooms, so that count was
invisible to clients — but it still sat in the table and still summed into
account-wide totals, and its successor room is where any unread state belongs.

The observed rejection is also the reason this ADR does not simply act on the
error the send returns. `404 M_UNKNOWN: Could not find event …` is scoped to the
**event**, not the room: it cannot be distinguished from "that particular event
is unknown here", and `M_UNKNOWN` is additionally what Synapse reports for its
own internal errors. A predicate keyed on the error kind would mark live rooms
gone during a homeserver outage.

## Decision

### Two steps: a cheap flag, then a verifying probe

`room_upstream_reconcile(account_id, room_id, state, detail, first_flagged_at,
updated_at)` holds one of two states per room:

- **`suspect`** — a room-scoped call was rejected upstream for a room the
  account believes it is joined to. Recorded by `SdkGateway`'s receipt path with
  no network access and no verdict, because one rejection proves nothing (see
  Context) and because the rejecting call has a latency budget to protect
  (`sync.ephemeral_send_timeout_secs`). A repeat rejection is insert-if-absent:
  it must not reset `first_flagged_at`, which is how long the room has looked
  wrong.
- **`gone`** — the unread watcher's `reconcile_upstream_rooms` probed the room
  and the homeserver rejected *that* too.

Splitting the steps also makes the flow crash-safe, per the repo's multistep-flow
guardrail: the suspicion is durable, so a process killed between flagging and
probing resumes on the next boot rather than losing the signal.

### The probe: the smallest room-scoped read, classified by status

One `m.room.create` state read (`GET /rooms/{id}/state/m.room.create/`) per
suspect room, bounded by `UPSTREAM_PROBE_TIMEOUT` and capped at
`UPSTREAM_PROBES_PER_SWEEP` rooms per re-sweep, with the remainder logged rather
than silently dropped. It runs on the ADR 0070 watcher's existing 5-minute tick,
immediately before the count sweep, so a room settled in one pass is zeroed in
the same pass.

Only a **client** rejection settles a room as absent: `403` (the server knows
the room but not us in it) or `404` (it knows no such room). Everything else is
inconclusive *by status, not by errcode* — `probe_proves_room_absent` exists as a
pure, unit-tested predicate precisely because the tempting version of this check
is wrong. A `5xx` is the homeserver failing rather than answering, a transport
error carries no status at all, and both leave the row `suspect` for the next
pass. An outage therefore delays a verdict instead of fabricating one.

**A probe settles into three outcomes, not two** (`ProbeVerdict`). The first
implementation classified into a `bool` "absent", which made *reachable* and
*inconclusive* the same value: an inconclusive result took the same branch as a
success and **cleared** the suspicion. A transient `502` on a genuinely purged
room therefore erased its row, destroyed its `first_flagged_at`, and dropped it
out of `suspect_upstream_rooms` — the only queue that is re-probed — while
logging *"suspect room answered upstream"*. Nothing but the user re-opening the
room would put it back, which is the unattended reconcile this ADR promises,
inverted. The predicate was right and unit-tested the whole time; the branch
consuming it was not tested at all, so it shipped green. "It answered" and "we
could not tell" are different facts, and only one of them is evidence.

### Pin to zero, don't skip; never delete content

A settled room's counts are **pinned to zero** through the ordinary write path
rather than skipped. Skipping would leave the wrong value already persisted in
place, which is the frozen badge this ADR exists to clear; zeroing keeps the row,
the watcher's dedup cache, and the `unread_counts.changed` frame consistent, and
the ADR 0070 `pending` guard is bypassed for it since a decrease was always
allowed. Tombstoned rooms are pinned by the same predicate
(`unread_suppression_reason`), on the same reasoning.

"Tombstoned" here means what `Store::list_rooms` means by it: an
`m.room.tombstone` state event is **present**, whatever its content. The first
implementation asked `Room::successor_room().is_some()` instead, which is a
strictly narrower question — that returns `None` when the tombstone's
`replacement_room` field is absent or the event has been redacted. Such a room is
already hidden from every client's room list while never being suppressed, so it
would accrue an invisible count into account-wide totals with no surface left to
clear it from: the exact failure this ADR exists to fix, reached by a different
route. The two predicates have to be read as one — if `list_rooms` ever changes
what hides a room, this changes with it.

That correspondence is currently held by this paragraph and nothing else:
`unread_suppression_reason` takes `bool`s, so its tests cannot see which question
the call site asked, and `axon-sync` has no seam for building a `Room` to ask it
of. Reverting to `successor_room().is_some()` would be green. Issue #164 tracks
the seam.

Nothing here deletes room content. `events`, `room_state`, and the room's place
in the room list are untouched, so history stays readable — a purged portal's
messages are often the only copy left. Deleting axon's copy was considered and
rejected: it is unrecoverable if a verdict is ever wrong, and the user's local
history is not the homeserver's to retract.

### Recovery is automatic, and derived from the table

A successful room-scoped call clears any row unconditionally — one indexed delete
that matches nothing in the normal case — so a room that comes back (a restored
purge, a re-created portal, an outage that resolved) recovers with no
intervention.

For that to actually un-suppress the room, the **table is the single source of
truth**: the watcher re-reads `rooms_gone_upstream` into its in-memory set at the
top of every re-sweep, rather than accumulating verdicts in memory. The probe
therefore records only durably and lets that refresh pick its verdict up in the
same pass.

The first implementation got this wrong, and review caught it: the in-memory set
was insert-only, so clearing a row had no effect on a running process and a
recovered room stayed pinned to zero until restart — the opposite of the
unattended recovery this section promises. An accumulate-only cache of a mutable
durable fact is the shape to avoid; suppression can only be as revocable as the
state it is read from. The cost of reading it fresh is one indexed query per
sweep against a table that is normally empty.

A room whose row is cleared between sweeps stays suppressed for the remainder of
that window (at most `UNREAD_COUNTS_RESWEEP`). The visible effect is a missing
unread badge for a few minutes on a room that just came back, which is the mild
direction to err in.

**A failed re-read keeps the previous set.** Re-reading the table every sweep is
what makes suppression revocable, but it also means a store error has to be told
apart from an empty answer: "no room is gone" and "we could not find out" arrive
at the same place. The first version returned an empty set for both, so one
transient pool timeout un-suppressed every confirmed room, and the sweep running
immediately after wrote each purged room's stale non-zero snapshot back and
broadcast it — the frozen badge, restored for a whole window. Only a successful
read replaces the set. At the initial seed there is no prior set to lose, so an
error there does start empty, and the first re-sweep corrects it.

**A success beats a probe that was already in flight.** `mark_room_upstream_gone`
promotes only a row that is still `suspect`, so a genuine room-scoped call that
succeeds while a probe is outstanding — the probe holds a round trip open for up
to `UPSTREAM_PROBE_TIMEOUT` — wins, and the returning verdict is discarded and
logged rather than written back over it. An unconditional upsert here would be
worse than a stale verdict: promoting a cleared room re-suppresses it *and* hides
it from every future probe, which read only `suspect` rows, leaving nothing but
another lucky success to undo it. Absence is the claim that has to be proven, so
losing this race is the correct direction.

## Consequences

- A room the homeserver has dropped stops showing a badge no one can clear, and
  a tombstoned room stops carrying a count clients can't even see. A room that
  comes back resumes carrying counts within one re-sweep, with no restart.
- One extra store write per `POST …/read`: a delete on success, an
  insert-if-absent on failure. The route is client-debounced and fires on room
  open, and both are single-row primary-key operations.
- Up to `UPSTREAM_PROBES_PER_SWEEP` upstream round trips per 5-minute sweep, and
  only while suspicion exists — which requires a *failed* send, so the normal
  steady state is zero probes and one indexed read of an empty partial index.
- A room absent upstream keeps its history and its place in the room list. That
  is deliberate, and it means the room is *inert rather than gone*: sends to it
  will keep failing. Surfacing that state to clients (a "this room no longer
  exists upstream" affordance) is follow-up work, not this ADR.
- `room_upstream_reconcile` is the first place axon records a belief about a
  room that came from probing rather than from sync. It is deliberately narrow —
  two states, one consumer — and not a general room-health model.
