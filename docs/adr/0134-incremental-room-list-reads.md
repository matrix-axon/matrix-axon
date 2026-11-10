# ADR 0134 — Incremental room-list reads

**Status:** Proposed. The server half is this ADR's PR; the web client half is a follow-up PR. Together they close [issue 279](https://github.com/matrix-axon/matrix-axon/issues/279) and [issue 1024](https://github.com/matrix-axon/matrix-axon/issues/1024).

## Context

`GET /v1/rooms` returns every room the account has, every time.
The web client calls it from `rooms.refresh()` on every cold start and again on every socket reconnect, and since [#421](https://github.com/matrix-axon/matrix-axon/issues/421) iOS reconnects on every return to the foreground.
The [ADR 0085](0085-web-offline-first-cache.md) cache paints the cached rows at once, but each refresh still downloads the whole list to replace them.

On the production account the list is 5,779 rooms, about 2.5 MB of JSON and 428 KB as sent with brotli.
On an iPhone under the 3G link conditioner, after the transport fixes of [#990](https://github.com/matrix-axon/matrix-axon/issues/990) and [#991](https://github.com/matrix-axon/matrix-axon/issues/991) and the incremental marker reads of [ADR 0131](0131-incremental-device-state-reads.md), every cold start and resume still spent 4–12 s of the link on it (issue 1024, build 0.1.27+08a00bbe, 2026-11-09):

| When                | `/v1/rooms` size | Server-side duration | Client `net` |
| ------------------- | ---------------- | -------------------- | ------------ |
| Cold start 16:14:44 | 428,821 B        | 7.38 s               | 13.8 s       |
| Resume 16:16:49     | 428,821 B        | 12.02 s              | —            |
| Cold start 16:17:35 | 428,319 B        | 7.25 s               | 13.6 s       |
| Resume 16:20:39     | 428,318 B        | 4.06 s               | —            |
| Resume 16:21:47     | 428,318 B        | 4.39 s               | —            |

In the 2026-11-07 captures, before ADR 0131, the list hit its 20 s deadline on most cold starts, and each retry pushed the whole list down the link again ahead of the room being opened.
It is now the only large transfer at boot.

Almost none of it is news to the client.
In the production database on 2026-11-10, one listed room changed in the last 10 minutes and nine in the last hour.

Two approaches were considered and set aside:

- **Pagination** breaks the room-list cache's reconcile contract: a room absent from a page stops meaning the room is gone, so a room left on another client is never pruned. It also makes offline filtering partial, the failure ADR 0085 rejected a sliced cache for. Issue 279 lists the consumers that assume a whole list.
- **A conditional GET** (`ETag` and `If-None-Match`, issue 279's proposal) saves the download only when nothing at all has changed. At nine changed rooms an hour, a resume after a few minutes usually sees at least one change, and any one change costs the whole 428 KB again. A delta that finds nothing is about as small as a `304`, and a delta that finds a few rooms costs a few hundred bytes each instead of the whole list.

## Decision

### A `since` cursor on the existing GET

Every `GET /v1/rooms` response now carries a `cursor`, an opaque string.
A client that passes it back as `?since=<cursor>` receives only the rooms whose row changed since that read.
The response says which kind of read it is with `full`:

- `full: true`: `data` is every listed room, exactly as before. The client replaces what it holds. `removed` is empty.
- `full: false`: `data` holds only the listed rooms that changed since the cursor, each a complete `RoomDto`. `removed` names the rooms that were listed and no longer are. The client merges both into what it holds.

Either way the response carries `total`, the number of rooms the list holds at this read, in the same scope (`account_id`) as the request.

The new fields sit beside `data`, not inside it.
`data` stays the array of rooms that every existing client reads, so a read without `since` is unchanged apart from three extra top-level fields.
Moving the rooms into an object under `data` would break every existing client, and the TUI is an independently distributed binary.
This is additive under [ADR 0099](0099-openapi-compatibility-guard.md).

The cursor has the same form as ADR 0131's: the database clock at the read, in microseconds.
Clients must not parse it, and its format can change.
A malformed `since` is a `400`.
A `since` later than the database clock did not come from this clock, so the read is answered in full rather than trusted.

### What counts as a change

A `RoomDto` is built from four sources: the room's `room_summaries` row, its `room_unread_counts` row, its room-scoped `m.tag` account data, and the account's global `m.direct` map.
Whether the room is listed at all also depends on its account's state.
A room is in a delta when any of the four changed for it, or its account entered or left `active`.

**The summary row** gains a column, `list_changed_at`.
A trigger sets it to `clock_timestamp()` when an update changes a column that `RoomDto` shows: `last_activity_ts`, `last_event_id`, `name`, `topic`, `avatar_url`, `canonical_alias`, `room_type`, `display_name`, `hidden_left` or `hidden_tombstoned`.
A new row takes the clock as its default.

The row's existing `updated_at` is not used, because writes the list does not show also bump it.
After the production server restarted at 22:35 on 2026-11-10, `updated_at` moved on 5,869 summary rows within eight minutes.
None of those rows had new activity or new member counts.
Every room-state write calls `refresh_room_summary_display`, which rewrites the display columns whether or not they changed, and sync re-delivers room state after a restart.
A delta keyed on `updated_at` would resend the whole list after every restart.
The trigger compares old and new values, so a rewrite with the same values, a member-count write or a membership-epoch advance leaves `list_changed_at` alone.

**Unread counts and tags** use their own rows' `updated_at`, which a trigger already maintains.
Their writers are single statements, and adding a write to `room_summaries` on every unread change would make the hottest write path wait on the summary row's lock.
Deleting an unread row (`delete_stale_room_unread_counts`) leaves no row to carry a time, so a trigger on that delete bumps the room's `list_changed_at` instead.
Tag rows are updated in place and never deleted, except with the account.

**`m.direct`** is a single global value per account, and a change to it can flip `is_direct` on many rooms.
A trigger on its write compares the room ids in the old and new maps and bumps `list_changed_at` on the rooms whose membership in the map changed.
It reads the map the way `room_ids_in_direct_map` does: every string in every array value of the top-level object.
A rewrite that leaves the set of room ids unchanged bumps nothing.

**The account's state.** An account's rooms are listed only while the account is `active`, and a change of state touches none of the rows above.
`accounts` gains its own `list_changed_at`, which a trigger stamps when the state moves into or out of `active`; a move between two other states lists nothing either way and stamps nothing.
A delta reads it beside the room's own stamps, so every row of an account that changed state is in the delta: as a room when the account became active, in `removed` when it stopped.
State changes are rare, so repeating a whole account's rows for one overlap window afterwards is cheap; stamping each of its summary rows instead would take a row lock on all of them.

So a delta is the scoped rows where `GREATEST(list_changed_at, account list_changed_at, unread updated_at, tag updated_at)` is after the cursor, less the overlap below.

### Removals

A room leaves the list in one of two ways.

- **Hidden** (left, tombstoned, or its account no longer active): its row stays, and the hidden flag's change bumps `list_changed_at`, or the account's state change bumps the account's. The delta reports it in `removed`.
- **Gone** (the row was deleted by a purge or a summary rebuild, or with its account): nothing is left to carry a time, so the delta cannot report it.

`total` covers the second case.
The client's set after merging is a superset of the true list whenever every addition was reported, and every addition is: a new or un-hidden row carries a fresh `list_changed_at`, and a row of an account that became active carries its account's.
So the merged set has `total` rooms exactly when nothing went missing, and a mismatch means a room was removed that the delta could not name.
The client then reads in full.
A purge or rebuild of a room that is still listed is rare, and deleting an account is rarer; these cost one full read.

The count is only a safety net for removals.
It cannot stand in for an unreported addition, because one unreported addition and one unreported removal leave `total` unchanged and the list wrong.
That is why a change of account state carries its own stamp rather than relying on the count.
Writing a tombstone table to name them would cost a trigger on every summary delete and a purge horizon for the tombstones, for an event that happens a few times in an install's life.

### The cursor overlaps the write window

As in ADR 0131, a write's stamp is taken before its transaction commits, so a write that is stamped before a read's clock but commits after the read is invisible to that read.
A delta therefore returns everything changed after `cursor − overlap`.

Room-list writes are not ADR 0131's single autocommit statements.
Event ingest touches the summary row inside a transaction, and the summary rebuild runs on the maintenance pool as one transaction of six statements, each bounded by `maintenance_statement_secs` (default 120 s).
A rebuilt row is stamped when its insert runs and commits after up to four more statements.
The overlap is four times the maintenance statement timeout, with a floor of fifteen minutes.
The triggers stamp with `clock_timestamp()` rather than `now()`, so a row's stamp is the time of the statement that wrote it, not its transaction's start.

The cost is small: every delta repeats the rooms changed in the fifteen minutes before its cursor, which in the production measurement above is one or two rooms, and the client's merge treats them as no-ops.
After a bulk change, such as marking every room read, deltas repeat that batch for fifteen minutes.

A transaction-id watermark was rejected for ADR 0131's reason: cursors would not survive a dump and restore into another cluster.

### The delta reads in one statement

The delta's rows, its `removed` rooms and `total` come from one statement, so they share one snapshot and the count cannot disagree with the rows.
The clock is read in a statement before it, as in ADR 0131, so the snapshot is never older than the cursor it hands out.
`is_direct` is applied from a separate read of `m.direct` afterwards, as the full read already does; an `m.direct` write that lands between the two bumped its rooms after the clock and is in the next delta.

### No new index

The delta scans the scoped summary rows and probes the unread and tag rows by primary key.
On the production database (15,383 summary rows, 5,779 listed), the delta statement ran in about 27 ms, against 13 ms for the full list's statement.
Reading inactive accounts' rows too, so their rooms can be reported as removed, added about a tenth to that on the same data.
An index on `list_changed_at` would cost every summary write to save some milliseconds per reconnect, so this ADR adds none.

### The web client keeps the server's list and its cursor

This is the follow-up web PR.

The client keeps the server's view of the list, from responses only, with its cursor, and sends `since` on every refresh it has a cursor for.
A delta is merged into that view: each room in `data` replaces or adds its row, each room in `removed` is dropped.
The merged view is then handed to the existing refresh path as though it were a full response, so the pending-leave, pending-display-name, local-metadata and metadata-sequence rules in `rooms.ts` apply to it unchanged.
Live patches and local writes are not merged into the server's view.
A live frame describes a change the server made after the cursor, so it is in the next delta; a local write the server never accepted must not be kept as though it had.

If the merged view does not hold `total` rooms, the client drops the cursor and reads in full.
A `400` to a read with `since` drops the cursor too, and the client reads in full once; a timeout or a `5xx` keeps it for the retry.
A `200` without a cursor, from an older server, is a full read.

The server's view and its cursor are persisted in the ADR 0085 cache, under the room-list cache's setting and through the same field allow-list, so a cold start's first refresh is a delta.
They are a record of their own, beside the rows the list paints from.
The painted record keeps carrying live patches, which ADR 0085 needs so that a rename seen live survives an offline restart; the server-view record never carries them.
A delta with no changes leaves both records alone once the server-view record is on disk, so most resumes write nothing.
The first delta after the cache was off or cleared writes the record even if it changed nothing, or the next cold start would read in full again.

## Consequences

- A client that keeps the list and cursor across launches downloads the whole list once per install, then a few hundred bytes per changed room on each cold start or resume.
- A client that keeps nothing across launches still saves every refresh after its first.
- Clients that never send `since`, such as the TUI, get the same list as before.
- The first full read on a new install is still the whole list. Issue 279's comment about re-requesting it while an earlier attempt is still downloading still applies to that one read.
- A future write path that changes a listed value, or that brings a room into the list, must go through `room_summaries`, `room_unread_counts`, the account-data rows or `accounts.state`, or it must bump `list_changed_at` itself. A future condition on which rooms are listed needs its own stamp, for the reason in "Removals". A new field in `RoomDto` must be added to the trigger's column list, or a change to that field alone never reaches a delta. The trigger's migration and `Store::list_rooms` say this beside each other.
- A writer whose transaction stays open longer than the overlap after its first listed write can be missed by a delta. The rebuild is the longest writer today; raising `maintenance_statement_secs` widens the overlap in step.
- Restoring the database from an older backup loses changes newer than the backup. A client's next delta is then answered from the restored rows, as ADR 0131 accepts for markers, and its next full read corrects it.
