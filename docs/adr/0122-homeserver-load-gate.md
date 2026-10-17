# ADR 0122 — A load gate between axon's background work and the homeserver

Status: proposed (#749)

## Context

On 2026-10-17 two axon instances starved a production Synapse for about nine
hours (#741). ADR 0121 (PR #746) fixes the backfill loop that did it. This ADR
is about the general case: nothing in axon stops _any_ background caller from
doing the same thing.

The incident data shows why a request-rate limit would not have helped:

- **The rate was low.** About 2,111 `/messages` calls over the incident, roughly
  one every 15 s across both instances. Any rate limit loose enough to let
  backfill make progress allows that.
- **Each request was expensive.** A page of a large space makes Synapse fetch
  history over federation and resolve state.
- **Abandoned requests kept running.** 1,016 of the calls were logged `200!`:
  Synapse finished them after axon had timed out and disconnected. Axon's
  timeout ends axon's wait, not the homeserver's work. Every timeout followed by
  a new request therefore added work in flight on Synapse, while axon believed
  it had one request outstanding.

What has to be bounded is the work the homeserver is doing on axon's behalf, and
the only signal axon has for that is how the homeserver responds: latency,
timeouts, 5xx and 429.

### Who calls the homeserver without a user waiting

Inventory of axon's own background requests as of this ADR, each with its own
local bound and none aware of the others:

| Caller                                                          | Request                      | Local bound today                                    |
| --------------------------------------------------------------- | ---------------------------- | ---------------------------------------------------- |
| Backfill (`backfill::run`, ADR 0043/0121)                       | `GET /messages`              | One page at a time per account; 30 s page timeout    |
| Upstream room probe (`reconcile_upstream_rooms`, ADR 0090)      | `GET /state/m.room.create`   | 8 rooms per sweep; 15 s timeout; SDK retries left on |
| Re-decryption sweep (`redecrypt::sweep_pending_utds`)           | key backup download per room | Sequential; only the SDK's default 30 s timeout      |
| Known-contact DM auto-join (`join_candidate_invites`, ADR 0040) | `POST /join`                 | Sequential; rare                                     |

Out of axon's reach: the sync long-poll and the requests matrix-sdk makes
internally (key upload and query, to-device, backup bookkeeping). Requests a
user is waiting on — sends, receipts, leave, room-entry, media downloads — are
interactive; media downloads already share a 16-slot semaphore.

### Standard patterns

- **Fixed concurrency cap ("bulkhead").** matrix-sdk offers one:
  `RequestConfig::max_concurrent_requests`, a single semaphore over every
  request the client sends. Axon does not set it, and it is the wrong tool on
  its own: the sync long-poll would hold a slot permanently, it cannot rank
  background work below a user's own actions, and it frees the slot when axon
  times out, while the homeserver is still busy.
- **Circuit breaker.** Stop all traffic after a failure threshold, then let one
  probe through after a cooldown (Hystrix, resilience4j). Simple and
  predictable, but binary.
- **Adaptive concurrency limit.** Additive increase on success, multiplicative
  decrease on a timeout or slow response — TCP congestion control applied to
  requests (Netflix `concurrency-limits`, Envoy's adaptive concurrency filter).
  Its decisive property here: independent clients converge on a fair share of a
  struggling server **without coordinating**. The three axon instances on
  bostoncoop.net share one Synapse and cannot talk to each other.
- **Retry budgets and `Retry-After`.** matrix-sdk already honours 429
  `Retry-After`; ADR 0121 adds per-room backoff to backfill.

## Decision

### 1. One gate per homeserver, for background requests only

`axon-sync` gains a `HomeserverGate`, one per homeserver base URL, shared by
every account in the process that uses that homeserver. Every request in the
inventory above takes a permit from it before sending:

```rust
let permit = gate.acquire(&cancel).await?;   // waits; cancellation-aware
let result = send(...).await;                 // the caller's own timeout
permit.finish(Outcome::of(&result, elapsed)); // feeds the limit
```

- Waiting for a permit is not part of the request: the caller's timeout starts
  after `acquire` returns, and waiting is never recorded as a failure (so ADR
  0121's per-room backoff and parking are unaffected by a closed gate).
- A gated request disables matrix-sdk's own retry (as ADR 0121 does for
  backfill). Otherwise one permit could cover up to 15 minutes of SDK retries
  against a 5xx.
- A permit dropped without `finish` (cancellation, shutdown) releases its slot
  and teaches the gate nothing.

Requests a user is waiting on never wait on the gate. Where axon already times
them out (the gateway's `timed_out()` sites, ADR 0121 decision 5), they report
their outcome so that a struggling homeserver slows background work first. They
only ever _decrease_ the limit; increases come from gated requests alone.

### 2. The limit adapts (AIMD)

Each completed gated request is classified:

| Outcome                                                       | Effect               |
| ------------------------------------------------------------- | -------------------- |
| Success under `SLOW_AFTER`                                    | counts toward growth |
| Success over `SLOW_AFTER`; timeout; 5xx; 429; transport error | halve the limit      |
| Any other 4xx                                                 | none                 |

- The limit starts at `INITIAL_LIMIT` and grows by one after `GROW_AFTER`
  consecutive fast successes, up to `MAX_LIMIT`.
- Halving rounds down, so a limit of 1 halves to 0, which opens the breaker
  (decision 3).
- An outcome only halves the limit if its request started after the most recent
  decrease. One burst of slow responses from requests that were already in
  flight counts as one signal, not several — the same rule TCP applies per
  round trip.

### 3. At zero, a circuit breaker

- **Open:** no gated request is sent. WARN once with the homeserver and the
  outcome that opened it.
- After `COOLDOWN` (doubling on each consecutive reopen, capped at
  `COOLDOWN_MAX`) the gate admits **one** probe request.
- A fast success closes the breaker at a limit of 1 and logs INFO. Anything that
  would halve the limit reopens it.

### 4. A timed-out request keeps its slot

When a gated request times out, its permit is not returned immediately. The slot
stays occupied for `TIMEOUT_HOLD`, on the assumption that the homeserver is
still working on the request — the `200!` lines in the incident log are that
work. This is the piece that targets what actually happened on 2026-10-17: a
client that frees its slot on timeout keeps adding to the homeserver's load at
exactly the moment the homeserver is overloaded.

### 5. Constants

In keeping with ADR 0121 decision 3, these are constants, not configuration:

| Constant        | Value  | Why                                                                                 |
| --------------- | ------ | ----------------------------------------------------------------------------------- |
| `INITIAL_LIMIT` | 2      | Backfill already pages one room at a time per account; 2 lets a probe run beside it |
| `MAX_LIMIT`     | 4      | Background work is never urgent; a small ceiling bounds the worst case per instance |
| `GROW_AFTER`    | 10     | Slow growth, so a recovering homeserver is not hit with the full limit at once      |
| `SLOW_AFTER`    | 10 s   | A healthy `/messages` page or state read answers in well under a second             |
| `TIMEOUT_HOLD`  | 2 min  | Four times the 30 s backfill page timeout                                           |
| `COOLDOWN`      | 1 min  | Matches ADR 0121's backfill retry base                                              |
| `COOLDOWN_MAX`  | 30 min | Recovery is noticed within half an hour of an outage ending                         |

### 6. Visible in `GET /v1/status`

Status gains a `homeservers` array: base URL, current limit, permits in use,
permits held by timeouts, and breaker state (`closed`, `open` with when it
opened, `probing`). This is in-memory state, so it adds no query to an endpoint
that is already expensive (see the `backfill_progress` caveat on `/v1/status`).

## Consequences

- With this gate in place, the 2026-10-17 incident ends within minutes: the
  first few timeouts halve the limit to zero, each timed-out slot stays held,
  and background traffic drops to one probe per cooldown until Synapse answers
  quickly again.
- Several axon instances on one homeserver back off together without any
  coordination, and the homeserver's response time sets how much background
  work it gets.
- Background work slows whenever the homeserver is slow for any reason, including
  load axon did not cause. That is intended: background work is the first thing
  that should yield.
- A homeserver that is slow but healthy (a small VPS, a busy matrix.org) settles
  at a low limit and backfills slowly. Backfill already tolerates that.
- The gate cannot protect the homeserver from other clients, from users'
  own interactive load, or from the requests matrix-sdk makes internally. The
  homeserver's own defences — Synapse's rate limits, workers, reverse-proxy
  limits — remain the operator's job. As far as we know, Synapse's built-in
  rate limits cover sending, joins and logins, not reads such as `/messages`.
- The gate gives no fairness between background callers: permits are FIFO. If
  backfill turns out to crowd out the probe or the re-decryption sweep, a
  priority order can be added inside the gate without changing callers.

### Alternatives rejected

- **matrix-sdk's `max_concurrent_requests`:** covers sync and interactive
  traffic too, so a low value delays a user's sends behind backfill, and a high
  one bounds nothing; frees the slot on timeout.
- **A request-rate limiter (`governor`, already a dependency):** measures the
  wrong thing; the incident's rate was well inside any plausible quota.
- **A local proxy in front of the homeserver:** would see the sync long-poll
  and every SDK-internal request, but cannot tell background from interactive
  traffic, and adds a deployment component.
- **Relying on the homeserver to cancel abandoned requests:** the incident log
  shows Synapse completing `/messages` after the client disconnected; the gate
  cannot assume otherwise.

## Delivery

Each step is one PR in one silo, after #746 merges:

1. **sync:** the `HomeserverGate` with deterministic tests under paused tokio
   time (limit growth, the start-after-last-decrease rule, the breaker cycle,
   the timeout hold). Backfill, the upstream probe, the re-decryption sweep and
   DM auto-join acquire it. `GET /v1/status` gains `homeservers` (an additive
   OpenAPI change, carried in the same PR as ADR 0121's `rooms_parked`).
2. **server:** the gateway's interactive timeouts and 5xx feed the gate. This
   lands after ADR 0121's server step, which changes the same `timed_out()`
   sites.
