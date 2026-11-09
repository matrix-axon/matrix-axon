# ADR 0131 — Incremental device-state reads

**Status:** Accepted. The server half shipped in [#1018](https://github.com/matrix-axon/matrix-axon/pull/1018); the web client half is [issue 280](https://github.com/matrix-axon/matrix-axon/issues/280)'s follow-up PR.

## Context

[ADR 0048](0048-device-state-drafts-and-read-markers.md) gives clients one read of a device-state namespace: `GET /v1/devices/{device_id}/state/{namespace}` returns the whole merged namespace.
The web client reads `drafts`, `read_markers` and `thread_read_markers` on the first room open.
It reads every namespace it holds again on each socket reconnect, because the bus is lossy ([ADR 0061](0061-web-live-websocket-layer.md)).
Since [#421](https://github.com/matrix-axon/matrix-axon/issues/421), iOS reconnects on every return to the foreground.

The namespaces have grown with the account.
Measured on 2026-11-08 against the production account with 5,760 rooms:

| Namespace             | Live keys | Written in the last day / week / month | As sent (brotli)      |
| --------------------- | --------- | -------------------------------------- | --------------------- |
| `read_markers`        | 6,254     | 31 / 129 / 378                         | 1,452,603 B (~411 KB) |
| `thread_read_markers` | 1,122     | 8 / 74 / 269                           | 461,345 B (~115 KB)   |

Smaller accounts have the same shape: about 45 room markers but 740–780 thread markers, ~79 KB compressed per read.

On an iPhone under the 3G link conditioner, with the transport fixes of [#990](https://github.com/matrix-axon/matrix-axon/issues/990) and [#991](https://github.com/matrix-axon/matrix-axon/issues/991), three sessions and four resumes showed:

- The room page paints before the markers arrive, so the markers no longer gate the timeline.
- On every cold start and every resume, `read_markers` and `GET /v1/rooms` (~429 KB) started together and finished within a second of each other in the server's access log. HTTP/2 interleaves them, so the markers take half of the room list's bandwidth.
- `/v1/rooms` hit its 20 s deadline on two of three cold starts and on one resume. In one session `read_markers` itself hit the deadline at cold start and again on resume, so the client never had read markers at all.
- Each resume re-downloaded ~526 KB of markers to learn of roughly thirty changes a day.

So nearly all of every re-read is entries the client already holds.

## Decision

### A `since` cursor on the existing GET

Every read now returns a `cursor`, an opaque string.
A client that passes it back as `?since=<cursor>` receives only the keys written since that read, not the whole namespace.
The response says which kind of read it is with `full`:

- `full: true`: `entries` is the whole merged namespace, with deleted keys absent, exactly as before. The client replaces what it holds for that namespace.
- `full: false`: `entries` holds only the keys with a write since the cursor. Each one carries its current winner, and a key whose winner is a deletion carries `value: null`. The client merges these into what it holds.

A read without `since` is unchanged apart from the two new fields, so existing clients (the TUI, older web builds) are unaffected.
This is additive under the `/v1/` compatibility rule ([ADR 0099](0099-openapi-compatibility-guard.md)).

The cursor is not a sequence number the client may do arithmetic on.
Today it is the server's microsecond clock at the read; its format can change without notice.
A malformed `since` is a `400`.

### A delta returns current winners, not raw rows

A delta selects the keys that have any row written after the cursor, then computes each key's winner across all the account's devices, the same `DISTINCT ON` as the full read.
The changed keys come from the cursor, but each winner is computed over the key's full row set.
A delta's answer for a key is then identical to what a full read would say, whatever the merge's ordering and tie-break rules become.
Merging only the rows written after the cursor would be correct today, because last-write-wins makes the newest row the winner, but only for as long as that rule holds.

Tombstones are returned in a delta, because a client that already holds a key needs to learn it was deleted.
They are still omitted from a full read, where an absent key already means "not set".

### The cursor overlaps the write window

The cursor is the database clock taken just before the read.
A write's `updated_at` is its own transaction's `now()`, which is taken when the statement starts, not when it commits.
A write that started before the read but committed after it carries an `updated_at` older than the cursor, and the read did not see it.
A strict `updated_at > cursor` would miss it forever.

So a delta returns every key written after `cursor − overlap`.
Every device-state write is one autocommit statement, bounded by the ordinary pool's `statement_timeout` (`database.timeouts.statement_secs`, default 10 s, which also bounds its lock waits).
The overlap is twice that timeout, with a floor of five minutes, so a write can be missed only if its commit lands more than the overlap after its statement started.
This rests on two invariants of the writers.
Each write must stay a single statement outside any wider transaction, because `now()` is the transaction's start, so a transaction held open across statements could commit later than the overlap allows.
And only the ordinary pool writes device state; the maintenance and index pools have longer statement timeouts.
Deleting an account removes its rows by cascade, which needs no cursor.
The cost of the overlap is a few re-sent keys per delta, which the client's merge treats as no-ops.

A transaction-id watermark (`pg_snapshot_xmin`) would close this window exactly.
It was not chosen because transaction ids belong to the cluster, not the data.
After a dump is restored into another cluster, for a host move or a major-version upgrade, every outstanding cursor would be ahead of all new writes and would silently skip them.
Timestamps survive a dump and restore.

### A cursor from the future falls back to a full read

If `since` is later than the database clock, the cursor did not come from this clock.
The server's clock might have stepped backwards, or the cursor might come from another deployment.
The read is then served in full, with `full: true`, rather than as a delta that could miss writes.

### No new index

The delta is a scan of one `(account, namespace)` range of `device_state_merge_idx` plus an index probe per changed key.
On the production account above, with 8,086 `read_markers` rows, a delta over a day's writes ran in 12.6 ms, against 31 ms for the full read.
An index on `updated_at` would cost every marker write to save a few milliseconds per reconnect, so this ADR adds none.

### The web client keeps the marker namespaces and their cursor

The web client sends `since` on every read of a scope it holds a cursor for, the reconnect re-read included.
It applies `full: true` as a replacement, removing keys the response omits, and `full: false` as a merge, removing keys that come back as `null`.
Either way a key with a local write the server has not acknowledged keeps its local value, the rule every read already follows.
So does a key a sibling's frame changed after the read was sent, because the server's snapshot predates that frame.
The cursor still covers a key a read left alone, so no later delta resends it unless it changes again; the next read that may apply it sets it from the server's view instead, unless that read carries the key itself.
Reads of one scope run one at a time, so a slow full read cannot land over the delta that followed it, and a read asked for while another is queued and unsent joins it.
A `400` to a read with `since` means the server will not take that cursor, after a format change or a bad record.
The client then drops the cursor, on disk too, and reads in full once; a timeout or a `5xx` keeps the cursor for the retry.
A `200` without a cursor, from an older server, drops the cursor on disk as well.

`read_markers` and `thread_read_markers` are also persisted, with their cursor, in the [ADR 0085](0085-web-offline-first-cache.md) cache.
A cold start restores them before its first read, so that read is a delta too.
What is persisted is the server's view as of the cursor, from read responses only, never the local cache.
A local write or a sibling's frame lands after the cursor and so reappears in the next delta; persisting it would store a value the server may never have accepted.
A delta with no entries leaves the record alone, so most resumes write nothing to disk.

The markers are metadata, event ids and timestamps, of the same class as the room list, so they follow the room-list cache's setting, on by default, and are wiped with it.
`drafts` is not persisted: a draft is message text the user typed, and ADR 0085 keeps plaintext off the disk unless the user opts in.
It is a few keys, so its full read on a cold start costs little; it still uses `since` on reconnect.

A marker scope with no cursor, on the first launch or with the cache off, waits for the room list's first refresh to settle before its full read.
Every read of the scope waits, the reconnect re-read included, so a socket drop during the wait cannot start the download early.
That full read is the 411 KB download measured above, and sharing the link with `GET /v1/rooms` is what pushed the room list past its deadline.
The wait ends when the refresh settles, whether it succeeds or fails, so the markers are at most one room-list deadline late.
A scope restored with a cursor does not wait, because its read is a few hundred bytes.

`hydrated` still means a read has answered.
A restored scope shows its markers at once but is not hydrated until its delta lands, because the forward-only guard of a write must compare against the server's view, not a copy that may be days old.

## Consequences

- A client that keeps its namespaces and cursor across launches pays the full download once per install, then a few hundred bytes per cold start or resume. The web client does this for the marker namespaces while the room-list cache is on.
- A client that keeps nothing across launches still saves every reconnect re-read after the first.
- The overlap means a delta right after a bulk write, such as marking every room read, re-sends that batch for up to five minutes. That is bounded by what was written and is idempotent.
- Restoring the database from an older backup loses writes newer than the backup. A client holding them keeps them until its next full read, and its next write to that key re-creates the row. For markers and drafts that is benign; a namespace where a lost write matters should force a full read after a restore.
- Raising `statement_secs` widens the overlap in step, so the guarantee holds at any configured timeout.
- Tombstones are never purged today, so a cursor of any age is safe. A future purge must answer `full: true` for any cursor older than its purge horizon, or a client holding that cursor never learns of the purged deletions.
- A cursor is not tied to a database lineage. After a restore or a failover to a lagging replica, an old cursor that is still before the database clock is trusted. The writes the restore lost are lost to a full read too, so this trade-off is the one the restore consequence above accepts.
