# ADR 0122 — A load gate between axon's background work and the homeserver

Status: accepted (#749); step 1 implemented, as amended below

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
  it had one request outstanding. Synapse kept working on an abandoned request
  for a median of 18 minutes after axon gave up (decision 4 has the figures).

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

`axon-sync` gains a `HomeserverGate`, one per homeserver, shared by every
account in the process that uses that homeserver. Every request in the inventory
above takes a permit from it before sending:

```rust
let permit = gate.acquire(&cancel).await?;   // waits; cancellation-aware
let result = send(...).await;                 // the caller's own timeout
permit.finish(Outcome::of(&result, elapsed)); // feeds the limit
```

- **The key** is the base URL the SDK settled on after discovery
  (`Client::homeserver()`), reduced to its origin: lower-cased scheme and host,
  with the port made explicit. Two server names that delegate to the same base
  URL therefore share a gate. One homeserver reachable under two different base
  URLs (an internal and a public name, say) gets two gates; that is accepted,
  since each gate still backs off on its own signals.
- **Waiting for a permit is not part of the request:** the caller's timeout
  starts after `acquire` returns, and waiting is never recorded as a failure, so
  ADR 0121's per-room backoff and parking are unaffected by a closed gate.
- **One permit, one bounded request.** A permit must not cover matrix-sdk's own
  retries, which can run for up to 15 minutes against a 5xx:
  - Callers that send a request themselves (backfill, the upstream probe)
    disable SDK retry on that request with `with_request_config`, as ADR 0121
    does for backfill.
  - Callers that go through an SDK method sending with the client's default
    config (`download_room_keys_for_room`, `join_room_by_id`) cannot. They wrap
    the call in an outer `tokio::time::timeout` of 30 s. The SDK's own 30 s
    timeout applies to each attempt, not to the call, so it is the outer
    timeout that bounds the call and its retries. Dropping the future at the
    timeout ends the SDK's retry loop, so the permit's span is bounded and the
    timeout is reported as one. Within those 30 s the SDK may retry a 5xx a few
    times under one permit; that is accepted. Changing the client-wide config
    instead would also change sync and every interactive request.
  - The outer timeout covers the whole SDK call, so for
    `download_room_keys_for_room` it can fire while the downloaded keys are
    being imported into the crypto store. In matrix-sdk 0.19.1 that import is
    safe to cancel. The keys are written in one SQLite transaction on a
    blocking thread, so dropping the future either stops the write before it
    starts or lets it finish in the background, and never leaves it half
    written. What a drop can lose is the room-keys notification sent after the
    write, which `redecrypt::run` listens for. The sweep can also re-decrypt
    that room's rows before a background write lands. Either way those rows
    stay undecryptable until the next sweep, as they do today when a download
    fails. A timeout during the import is also recorded as a homeserver
    timeout. Both are accepted: the import is local and short next to a 30 s
    bound, so the timeout almost always fires during the request.
- **A permit dropped without `finish`** (cancellation, shutdown) releases its
  slot and teaches the gate nothing.

Requests a user is waiting on never wait on the gate. Where axon already times
them out (the gateway's `timed_out()` sites, ADR 0121 decision 5), they report
their outcome, with the time the request started, so that a struggling
homeserver slows background work first. Their effect is capped:

- they can only halve the limit, never grow it;
- they can take it no lower than 1, so they never open the breaker on their
  own, and they are ignored while the breaker is open;
- they hold no slot (decision 4), because they never had one.

The media proxy's downloads do not report: a slow remote media server says
nothing about the homeserver's load.

### 2. The limit adapts (AIMD)

Each completed gated request is classified by its outcome:

| Outcome                                           | Effect               |
| ------------------------------------------------- | -------------------- |
| Success in under half the request's own timeout   | counts toward growth |
| Slower success                                    | none                 |
| Timeout, 5xx, 429, transport error                | halve the limit      |
| Any other 4xx (the request's problem, not load's) | none                 |

- **Growth:** the limit starts at `INITIAL_LIMIT` and grows by one after
  `GROW_AFTER` fast successes since it last changed, up to `MAX_LIMIT`.
- **A slow success is neutral,** not a decrease. Slowness alone is a poor load
  signal here: in the incident log, completed pages for the space took a median
  of 0.84 s, but 4.4 % of them took over 10 s (p99 14.3 s, max 23.4 s) while
  still succeeding. Measuring "fast" against half of each request's own timeout
  (15 s for a backfill page, 7.5 s for the upstream probe) keeps the threshold
  per request class, and only about 1 % of the incident's completed pages would
  have missed it.
- **One burst counts once:** an outcome only halves the limit if its request
  started after the most recent decrease. Requests that were already in flight
  when the limit dropped are the same signal, the rule TCP applies per round
  trip.
- **Halving rounds down,** so at a limit of 1 a hard failure opens the breaker.
  That is intended: at 1 the gate is admitting a single request, and that
  request has just timed out or been refused by an overloaded server. Slow
  successes and other 4xx no longer halve, so this does not oscillate on a
  slow-but-healthy homeserver.

**Admission:** a request is admitted when the slots in use (active permits plus
held slots, decision 4) are fewer than the limit. When a halving leaves more
slots in use than the new limit, nothing is cancelled; nothing new is admitted
until the count falls below the limit.

### 3. At zero, a circuit breaker

The breaker has three states, and every outcome has a defined transition:

- **Closed:** the limit is at least 1 and decision 2 applies.
- **Open:** entered when the limit reaches 0. WARN once, naming the homeserver
  and the outcome. No gated request is admitted. The open period lasts
  `COOLDOWN × 2ⁿ`, capped at `COOLDOWN_MAX`, where _n_ is the number of times
  the breaker has reopened from **Probing** since it last closed. Closing
  resets _n_ to 0, so one outage does not leave a later one starting at 30
  minutes.
- **Probing:** entered when the open period is over **and** no held slots
  remain. Exactly one waiting request (the first in line) is admitted.

| Probe outcome             | Next state                                                |
| ------------------------- | --------------------------------------------------------- |
| Success, at any speed     | Closed at a limit of 1; INFO                              |
| Any other 4xx             | Closed at a limit of 1: the homeserver answered promptly  |
| Timeout                   | Open, _n_ + 1; the probe's slot is held (decision 4)      |
| 5xx, 429, transport error | Open, _n_ + 1                                             |
| Dropped without `finish`  | Open with its period already over: the next waiter probes |

Because the probe also waits for held slots to clear, **after timeouts the
recovery time is set by `TIMEOUT_HOLD`, not `COOLDOWN`.** `COOLDOWN` governs
only when the breaker opened on 5xx, 429 or transport errors, which hold no
slot.

### 4. A timed-out request keeps its slot

When a gated request times out, its permit is not returned. The slot stays
occupied for `TIMEOUT_HOLD`, on the assumption that the homeserver is still
working on the request. The incident log measures how long that is. Every one
of the 1,016 abandoned requests was dropped by axon at its 30 s timeout, and
Synapse then kept working on it for a further:

- 3 min at the 10th percentile;
- 18 min at the median;
- 2.2 h at the 90th percentile (max 2.4 h).

A client that frees its slot on timeout keeps adding work at exactly the moment
the homeserver is overloaded; that is what happened on 2026-10-17. No fixed hold
matches that distribution. A 15-minute `TIMEOUT_HOLD` outlasts the remaining
work of 43 % of the abandoned requests. Holding longer buys little: the
distribution is flat past the median, so 20 minutes would cover 51 % and 30
minutes only 56 %, while every extra minute of hold delays recovery from a
short slowdown. The longer tail is bounded by the breaker instead: with held
slots and a doubling cooldown, an unrecovered homeserver gets at most a few
probes an hour.

### 5. Constants

In keeping with ADR 0121 decision 3, these are constants, not configuration:

| Constant        | Value  | Why                                                                                          |
| --------------- | ------ | -------------------------------------------------------------------------------------------- |
| `INITIAL_LIMIT` | 2      | Backfill already pages one room at a time per account; 2 lets another caller run beside it   |
| `MAX_LIMIT`     | 4      | Background work is never urgent; a small ceiling bounds the worst case per instance          |
| `GROW_AFTER`    | 10     | Slow growth, so a recovering homeserver is not hit with the full limit at once               |
| `FAST_FRACTION` | ½      | A success counts toward growth in under half its own timeout (decision 2)                    |
| `TIMEOUT_HOLD`  | 15 min | Outlasts 43 % of the incident's post-timeout work; 30 min would cover only 56 % (decision 4) |
| `COOLDOWN`      | 1 min  | Applies only to an open caused by 5xx/429/transport errors; matches ADR 0121's retry base    |
| `COOLDOWN_MAX`  | 30 min | Recovery is noticed within half an hour of an outage ending                                  |

### 6. Visible in `GET /v1/status`

Status gains a `homeservers` array. Each entry has:

- the gate's key;
- the current limit, active permits and held slots;
- how many requests are waiting and how long the oldest has waited;
- the breaker state (`closed`, `open` with when it opened and when it may next
  probe, or `probing`).

This is in-memory state, so it adds no query to an endpoint that is already
expensive (see the `backfill_progress` caveat on `/v1/status`).

## Consequences

- **The incident's background load would have stopped within the first minute
  or two.** The access log for the space shows only backfill's `/messages`
  traffic (#741), at about 100 abandoned requests an hour (177 at the peak).
  With the gate, the first timeout halves the limit from 2 to 1. The second
  request was already in flight, so its timeout does not halve again (decision
  2), but both slots are held, and two held slots at a limit of 1 keep
  background traffic at zero for 15 minutes. The gate closes through its held
  slots, not the breaker. When the holds expire, one request is admitted. If
  it also times out, the limit reaches 0 and the breaker opens. From then on
  each probe waits out both its cooldown and the previous probe's held slot,
  so an unrecovered Synapse gets one request every 15 minutes, falling to one
  every 30 minutes as the cooldown doubles past the hold. The log was filtered
  to the space's room ID, so it says nothing about sync or SDK-internal
  traffic; if those added load, the gate does not reduce it.
- **Several axon instances on one homeserver back off together** without any
  coordination, and the homeserver's response time sets how much background
  work it gets.
- **Background work yields whenever the homeserver struggles,** for any reason,
  including load axon did not cause. That is intended: background work is the
  first thing that should give way.
- **A slow-but-healthy homeserver** (a small VPS, a busy matrix.org) keeps
  answering, so it never opens the breaker; it just grows the limit slowly.
  Backfill already tolerates that.
- **After an outage, background work resumes no sooner than 15 minutes** after
  the last timeout. Backfill, the probe and re-decryption can all wait that long.
- **The gate cannot protect the homeserver** from other clients, from users'
  own interactive load, or from the requests matrix-sdk makes internally. The
  homeserver's own defences — Synapse's rate limits, workers, reverse-proxy
  limits — remain the operator's job. As far as we know, Synapse's built-in
  rate limits cover sending, joins and logins, not reads such as `/messages`.
- **Permits are FIFO,** with no priority between background callers. Backfill
  pages one room at a time per account and queues again after each page, so
  another caller waits behind at most one page per account. When the breaker
  is open everything waits; `waiting` and the oldest wait in `/v1/status` make
  that visible. A priority order can be added inside the gate later without
  changing callers.

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
   time: limit growth, the start-after-last-decrease rule, admission when
   slots in use exceed the limit, every row of the probe table, the cooldown
   reset on close, the timeout hold, and the floor of 1 for interactive
   reports. Backfill, the upstream probe, the re-decryption sweep and
   DM auto-join acquire it. `GET /v1/status` gains `homeservers` (an additive
   OpenAPI change, carried in the same PR as ADR 0121's `rooms_parked`).
2. **server:** the gateway's interactive timeouts and 5xx feed the gate. This
   lands after ADR 0121's server step, which changes the same `timed_out()`
   sites.

## Amendment — step 1 as built (2026-10-24)

Building step 1 settled three things the decision above left open.

### A third kind of caller: long-polls

ADR 0124 added a background caller after this ADR: the image-pack transport, a
state-only Sliding Sync long-poll (30 s server-side, 90 s outer timeout). It
cannot take a permit. A poll is meant to sit at the homeserver for its whole
server-side timeout, so it would occupy one of the two starting slots all the
time and would never count as a fast success. It is a long-poll caller instead:

- before each poll it waits while the breaker is not closed, and only then;
- it holds no slot while polling, and its latency is never classified, so a
  poll never grows the limit;
- a 5xx, 429 or transport error halves the limit like any background request,
  and can open the breaker;
- an outer timeout halves the limit and leaves a held slot, because the
  homeserver may still be working on the abandoned poll;
- when the breaker may probe and no background request is waiting, the poll is
  the probe, and its outcome follows the probe table. Without this, a gate whose
  only remaining traffic is the long-poll (backfill complete, nothing to probe)
  would stay open for good.

### Recover and manual re-decryption are interactive

The key-backup download runs in the boot re-decryption sweep, and also in the
recover and manual re-decrypt sweeps, which a user is waiting on. Those two are
interactive: they never wait for the gate, and report a timeout or 5xx under
decision 1's cap. All three put the 30 s outer timeout around each room's
download, so a call that used to retry under the SDK for minutes now stops at
30 s and leaves that room's rows undecryptable until the next sweep.

### No caller blocks the account's sync loop on the gate

- **The boot re-decryption sweep** used to run inline in the account run,
  before the watchers started. Waiting there on a shut gate would have held
  back verification, invites and unread counts for as long as the gate stayed
  shut, so the sweep now runs as its own task, drained with the re-decryption
  queue.
- **DM auto-join** runs inside the sync loop's `select!`, so it takes a permit
  only if one is free at once and nobody is queued. Otherwise it leaves the
  invite for the next verification poll, five seconds later.
- **The upstream room probe** runs on the unread-count watcher's five-minute
  resweep. Waiting there would freeze unread counts while the gate is shut, so
  it takes permits the same way, and a pass that finds the gate shut leaves its
  remaining rooms suspect for the next one. Such a pass can still be the
  breaker's probe, when the breaker is due one and nothing is queued.

`GET /v1/status` reports long-polls waiting at the gate in `waiting`, beside
background requests.
