# ADR 0121 — Backfill failure classes, persisted backoff, and timeouts that are not failures

**Status:** Proposed; tracked in [issue 741](https://github.com/matrix-axon/matrix-axon/issues/741), [issue 742](https://github.com/matrix-axon/matrix-axon/issues/742) and [issue 743](https://github.com/matrix-axon/matrix-axon/issues/743).
Amends ADR 0043's "Resume token and exhaustion" section.

## Context

On 2026-10-17 a production Synapse was starved for about nine hours by Axon's history backfill.
Two Axon instances signed in to the same account were paging the Matrix Community space (`!iMZEhwCvbfeAYUxAjZ:t2l.io`, `#community:matrix.org`).
Synapse could not fill that room's history gaps over federation, so every `/messages` call at one point in its history started federation backfill that ran for 25 minutes on average.
Axon gave up on each call after `sync.backfill_page_timeout_secs` (30 s), and Synapse kept working on it.

The Synapse access log for that room on that day shows 2,111 `/messages` calls, split almost evenly between the two instances, and no 5xx responses.
1,016 of them were logged `200!`: Synapse finished the work and found the client had already disconnected.
The pattern was exact: three hung requests on the same token, one request with no `from`, a few fast pages back to the same token, three more hung requests.

That loop comes from ADR 0043.
To recover from a token the homeserver permanently rejects, the engine counts consecutive `/messages` failures per room and, after `MAX_TOKEN_RETRIES` (3), re-pages the room from the live timeline end.
It counts every failure the same way: a token rejection, a transport error, and a request that simply took longer than 30 s.
A room that is slow at one point therefore restarts from the end, walks quickly back to the same point, and hangs there again, forever.
Three more defects made it worse:

- The failure count is in memory, so a restart, or a second instance, starts from zero.
- Nothing delays a failing room's next attempt beyond the sweep cadence, and nothing ever stops retrying it.
- Every restart from the end re-counts the pages it re-fetches in `events_backfilled`.
  `@a:bostoncoop.net`'s cursor for that space read 189,170 events, while `events` held 2,983 rows for the room (issue 744).

The room was a space.
A space's timeline is almost entirely `m.room.member` and `m.space.child` state.
Axon's space hierarchy reads `room_state`, which only live sync writes; backfill writes `events` alone.
So paging a space's history cost the homeserver a great deal and gave Axon nothing it uses.

Leaving that space from the web client then failed twice with a 502 after `sync.membership_mutation_timeout_secs` (15 s).
Synapse wrote the first leave event about 65 s after the request and finished the request after 194 s.
The leave had succeeded; the 502 told the user it had not, and the retry doubled the homeserver's work.
Sends already report this case as a 504, because the call is still outstanding rather than having failed (issue 241).
`timed_out()` in `crates/axon-server/src/gateway.rs` reports every other upstream timeout as a 502: membership verbs, joins, room creation, read receipts, typing, media sends and settings writes.

matrix-sdk's own retries were checked and played no part.
With the default `RequestConfig`, the SDK retries 5xx and 429 responses with exponential backoff for up to 15 minutes of total delay, and does not retry network failures or its own timeout.
There were no 5xx responses here.

## Decision

### 1. Classify each failed backfill page

A failed `/messages` page falls into one of three classes, and each gets its own response:

| Class        | What it is                                                                      | Response                                                                      |
| ------------ | ------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| Rejected     | A 400 for a request that carried a `from` token                                 | Clear the saved token. The next sweep pages from the live end, with no delay. |
| Inaccessible | A 403 or 404                                                                    | Park the room at once (decision 3).                                           |
| Retryable    | Axon's page timeout, a transport error, any 5xx, a 429, a 401, or anything else | Retry the **same** token after a growing delay (decision 2).                  |

A 400 is a deterministic answer about the token, so one is enough; the ADR 0043 retry count existed only because failures could not be told apart.
A 400 for a request with no `from` says nothing about a token and is retryable.
Clearing the token on rejection loses nothing, because a rejected token is worthless.
A second rejection of a room's token also parks the room (decision 3).
The pages back down from the live end succeed and reset the failure count, so a homeserver that rejects every token at some depth would otherwise have Axon walk back to that depth again every sweep.
The room's rejection count is stored separately, and a successful page does not reset it.
A restart from the end now happens only after a rejection, so the deeper position is never thrown away because of slowness alone.
For the same reason, a sweep whose cursor read fails is skipped, not run with no cursors: every room would look new, page from its live end and overwrite its saved token.

### 2. Back off a retryable failure, persistently

The delay before retrying a room after its `n`th consecutive failure is 1 minute doubled `n − 1` times, capped at 1 hour.
The count, the class of the last failure and the time of the next attempt are stored on the room's `room_backfill` row, so they survive a restart.
A sweep skips any room whose next attempt is still in the future, without issuing a request.
A page that succeeds clears all of it.

The page timeout bounds the `/messages` request only, not the decryption of the page after it.
Decryption is local work, and a slow decrypt is not a homeserver failure.

The backfill `/messages` request disables the SDK's internal retry, so a 5xx is retried only on this schedule, not also up to several times inside the 30 s page timeout.

### 3. Park a room that keeps failing

After 5 consecutive failures, one inaccessible answer, or a second token rejection, the room is parked: its next attempt is 24 hours away and it is logged once at WARN with the account ID, room ID and failure class.
A parked room is retried once each park interval, and stays parked if that attempt fails.
A room at the configured target depth is not paged while that depth stands, so its failure state is cleared, and it is not counted as parked.
`GET /v1/status` reports the count of parked rooms per account (`rooms_parked`), so an operator can see that backfill has stopped somewhere rather than finding out from the homeserver.

Five failures at 1, 2, 4 and 8 minute gaps leave at most five abandoned requests running upstream before the room goes quiet.
In the incident each instance made about 100 an hour.

These values are constants, not configuration.
They bound how hard Axon may lean on a struggling homeserver, which is not a per-operator preference.

### 4. Do not backfill spaces

The sweep skips `m.space` rooms (`Room::is_space()`) and records them as done without any `/messages` call, so backfill status counts them as complete.
Space children and other space state keep coming from live sync.
Issue 745 tracks the general case of ordinary rooms whose history is almost all state, which needs measurements before choosing a threshold.

### 5. An upstream timeout is a 504, not a 502

When Axon stops waiting for the homeserver, it reports `SendError::Timeout`, a 504 `gateway_timeout`, for every upstream call, not just sends.
A 502 is kept for a homeserver that answered with an error.
The homeserver may still complete a request Axon gave up on, and a 502 tells the client it failed.

`leave` and `forget` move to `sync.room_entry_timeout_secs` (30 s), since a leave in a large room needs state resolution just as a join needs federation.
That reduces false timeouts but cannot remove them; a leave that took Synapse 65 s would still time out.
So the clients treat a 504 from leave or forget as "still pending" and let the room list settle it, instead of inviting an immediate retry.
Issue 627's split of the read-receipt and typing timeouts belongs to the same change.

### 6. Delivery

One silo per PR:

1. Sync and store: decisions 1–3 and this ADR (issue 741).
2. Sync: decision 4, stacked on 1, since it reuses the stored backfill state (issue 742).
3. Server: decision 5 and issue 627.
4. Web, then TUI: present a 504 from leave or forget as pending (issue 743).
   The web client learns of a completed leave only on a refresh or reconnect (issue 103), so it schedules a refresh after a 504.

## Consequences

- A homeserver that cannot serve a room's history costs it at most a handful of abandoned requests a day per Axon instance, not one every few minutes.
- A room that really is stuck stays visibly stuck: parked, counted in status and logged, rather than silently retried.
  Unparking it early needs a restart and a database edit; there is no API for it.
- Rooms whose tokens are rejected once recover exactly as before, without the three-strike delay.
  A room whose token is rejected again walks back from its live end at most once a day.
- Spaces no longer carry deep history in `events`.
  Space timelines show what live sync delivered.
- Clients that keyed retry logic off a 502 from these routes see a 504 instead; neither first-party client distinguishes the two today.
- `events_backfilled` still over-counts re-walked pages until issue 744 is fixed, though re-walks now happen only after a rejection.
