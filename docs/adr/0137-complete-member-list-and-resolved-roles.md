# ADR 0137 — Complete member list and resolved member roles

**Status:** Proposed; tracked in [issue 659](https://github.com/matrix-axon/matrix-axon/issues/659), part of [issue 653](https://github.com/matrix-axon/matrix-axon/issues/653).
Extends the acquisition machinery of [ADR 0126](0126-room-detail-acquisition-and-coverage.md) under the rules of [ADR 0111](0111-cached-progressive-room-metadata.md), amends ADR 0126 decision 4 with a second job lane, and settles [issue 324](https://github.com/matrix-axon/matrix-axon/issues/324).

## Context

`GET /v1/accounts/{account_id}/rooms/{room_id}/members` returns the `m.room.member` rows of `room_state`.
Sync fills those rows with lazily loaded members: Sliding Sync's `required_state` carries the members of the timeline's senders, plus every membership event the timeline itself delivers.
A large room therefore returns a handful of members while `member_counts` on `/info` reports hundreds.
The TUI's `/whereami` (ADR 0114, ADR 0135) and the web Room Information panel (ADR 0133) both say so ("N loaded of M joined"), but neither can list or search everyone.

Roles have a separate gap.
`/power_levels` copies the flat numeric fields of the SDK's `RoomPowerLevels` and drops `rules.privileged_creators`, so a room-v12 creator, who holds an infinite level and never appears in `users`, reads as `users_default` (issue 324).
`/metadata` exposes the configured `m.room.power_levels` content only.
Neither lets a client honestly show who is an admin or a moderator.

The web client reads the same unpaginated `/members` on every room open, to resolve senders' display names and to offer mention candidates (`stores/members.ts`, `mentions.ts`).
That read is on the room-open critical path, so it cannot be made to trigger upstream requests or grow with the room.

### Production survey

Measured on 2026-11-12 against the production Synapse and Axon databases of the three-account instance, read-only, counts only:

- One account is joined to about 5,800 rooms; the other two to about 85 each.
  Of the first account's rooms, 143 have a projected joined list shorter than the authoritative joined count, 78 of them with more than ten members.
  The other two accounts have one and two such rooms, none larger than eleven members.
- The largest joined room has 1,759 joined and 250 banned members, about 2,000 that have not left; the next has about 1,800, then about 940; every other room has fewer than 450.
- In those rooms the projection holds 276 of 1,759, 59 of 1,721, and 15 of 858 joined members.
- A current membership event in client format is about 385 bytes on average (per-room averages between 375 and 436), and a departed one about 330.
  The four largest rooms hold 130 to 1,104 departed members in current state, so the complete `GET /rooms/{roomId}/members` response is 25 to 97 percent larger than one that leaves them out.
  The largest response is about 960 KB for 2,583 entries; without departed members it would be about 770 KB.

The byte figures rebuild each event's client form from Synapse's stored federation JSON, so they are estimates; real responses can add `unsigned` fields.

## Decision

### 1. A `members` acquisition job fetches the complete list on demand

ADR 0126's per-account acquisition worker gains a fourth job kind, `members`, keyed by room like the others.
It sends one unfiltered `GET /_matrix/client/v3/rooms/{roomId}/members` with the account's access token through reqwest and the shared bounded body reader, after taking a `HomeserverGate` permit as background work.
The response is one consistent snapshot of the room's membership state: one event for every user who has a membership, departed members included.
Departed members are fetched because decision 2 compares each user's synced event with their fetched one, and a user missing from a filtered response has no event to compare.
At the measured sizes they add 25 to 97 percent.

`members` jobs run in their own lane on the account's worker, amending ADR 0126 decision 4's "one request at a time per account".
The lane has its own queue of `MEMBERS_QUEUE_CAPACITY`, keyed by room, which drops its oldest job when full, and at most one request in flight.
The existing lane is unchanged, so a slow or large member fetch can neither delay nor evict the alias, directory, and absence jobs of the room a user is looking at; an account has at most two requests in flight.
Everything else in decision 4 applies to the new lane unchanged: `JOB_INTERVAL` pacing, duplicate requests coalesced, `queued`/`running` in memory only, failure categories and backoff, joined rooms only, the homeserver gate, and the `membership_epoch` fence.
A result commits only if the account still exists and the room's epoch is unchanged since the job started.

The job is demand-driven.
A read of `/members/page` (decision 3) schedules it when the room has no snapshot or its snapshot has expired, and the row's `next_attempt_at` has passed; the read never waits.
Nothing schedules it in the background: with about 5,800 rooms on one account and 143 of them incomplete, filling every room would spend thousands of requests on rooms nobody opens.
The legacy `/members` read never schedules it.
Reads can still schedule one request per room per `MEMBERS_TTL`, so a client paging through many rooms would cause many fetches; a per-account budget on read-triggered jobs is [issue 1091](https://github.com/matrix-axon/matrix-axon/issues/1091).

The response is refused before parsing past `MAX_MEMBERS_RESPONSE_BYTES`, and a list with more than `MAX_SNAPSHOT_ENTRIES` entries, departed members included, is not stored.
Both outcomes record `too_large` for `MEMBERS_TTL`, so a room that is too big is not asked again on every read, and the list stays visibly partial (decision 3); it is never cut.
The entry cap is the one that binds: the byte cap admits `MAX_SNAPSHOT_ENTRIES` entries averaging up to about 520 bytes, above every measured per-room average.

Outcomes map onto ADR 0126's value statuses: `200` with a valid body is `available`; an invalid body is the `invalid_response` failure; `403` is `forbidden`; `too_large` as above; every other outcome is a failure that updates only the refresh fields.

### 2. The snapshot is stored apart from synced state and merged on read

A new table, `room_member_snapshots`, holds one row per account, room, and user: the membership event's ID and `origin_server_ts`, and its membership, display name, and avatar URL.
Departed members keep only the first three.
The room's acquisition row (kind `members` in `room_detail_acquisitions`) carries the status, `fetched_at`, `expires_at`, and refresh fields, as for the other kinds.
A commit replaces the room's snapshot rows and updates the acquisition row in one transaction.
Snapshot rows are deleted with the acquisition row: on account removal and room purge through the summary foreign key, and on every new membership epoch through ADR 0126's trigger.
They are also deleted when a refresh ends `too_large` or `forbidden`, which ADR 0126 treats as conclusive answers that replace the value; the list then returns to the projection alone.
A refresh that fails transiently keeps them (decision 3 says how `complete` reports that).

The snapshot is never written into `room_state`.
ADR 0111 keeps acquired data out of synced state, and a snapshot is a statement about the room at one moment, whereas `room_state` rows are kept current by sync.

A read merges the two per user by comparing events, never write times:

- **Both sources have the user, with the same event:** they agree.
- **Both have the user, with different events:** the event with the later `origin_server_ts` wins; on equal timestamps the snapshot wins, because it is the homeserver's resolved state.
  This is the ordering the `room_state` upsert already applies to sync's own replays.
- **Only `room_state` has the user:** that row wins.
  The snapshot holds every user with a membership at the time it was served, so a user it lacks had no membership then, and sync's row is newer.
- **Only the snapshot has the user:** the snapshot row is used.
- **No available snapshot:** the read is the projection alone, as today.

After the merge, a winning row whose membership is `leave`, or is not one the read asked for, is dropped before paging and before `total` is counted.

The rule depends on no clock of Axon's and no transaction timing.
A sync transaction that commits while the request is in flight is compared by its event, like any other.
An old event that sync delivers late or replays, such as a join for a user the snapshot shows as departed, has an earlier `origin_server_ts` than the snapshot's event and loses, so it cannot bring a departed member back.
A membership change that sync delivers shows at once, and a change sync never delivered, such as a join inside a timeline gap, shows after the next refresh; `MEMBERS_TTL` bounds that staleness.
`origin_server_ts` is set by each event's origin server, so it is not a total order across servers; the merge inherits that limit from the projection rather than adding a new one.

### 3. `GET …/rooms/{room_id}/members/page` is the paginated read; `/members` is unchanged

`GET /v1/accounts/{account_id}/rooms/{room_id}/members/page` takes:

| Parameter    | Meaning                                                                                                                                                          |
| ------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `membership` | Comma-separated subset of `join`, `invite`, `knock`, `ban`; default all four. `leave` is a `400`: the page lists current members only.                           |
| `q`          | Optional search; matches the folded display name or Matrix ID as a substring.                                                                                    |
| `min_level`  | Optional; only members whose resolved level is at least this. `"infinite"` levels match every value. A `409` `roles_unavailable` when levels cannot be resolved. |
| `limit`      | Page size, default 100, at most 500.                                                                                                                             |
| `cursor`     | Opaque `next_cursor` from the previous page.                                                                                                                     |

It returns:

```json
{
  "data": {
    "members": [
      {
        "user_id": "@alice:example.org",
        "membership": "join",
        "display_name": "Alice",
        "avatar_url": "mxc://example.org/abc",
        "power_level": "infinite",
        "is_creator": true
      }
    ],
    "next_cursor": "…",
    "total": 1759,
    "complete": true,
    "snapshot": {
      "status": "available",
      "fetched_at": 1762934400000,
      "expires_at": 1762938000000,
      "refresh": { "state": "idle", "failure": null, "next_attempt_at": null }
    },
    "roles": "resolved"
  }
}
```

- **Order:** membership group (`join`, `invite`, `knock`, `ban`), then the folded sort key (display name, or the Matrix ID when there is none), then user ID.
  Paging is by that key, so a change between pages neither repeats nor skips a member who kept their position, and a member whose key moved may appear on either side.
  Roles do not take part in the order; a client shows privileged members with `min_level`, for example as a separate section above the list.
- **Folding:** Unicode lowercase with Latin accents folded to their base letters and combining marks dropped, the rule the TUI's `/whereami` filter already applies (`fold_match_key` in `clients/tui/src/app/room_info.rs`), so the server's search agrees with what TUI users have.
  It is implemented in `axon-core`; the two copies are kept in step by shared test vectors.
  Keys are computed on every read and never stored, so a change to the rule cannot leave old and new keys side by side within one listing.
- **`total`:** members matching the filters across all pages, so a client can say "12 of 340".
- **`complete`:** `true` while the room has an `available` snapshot in the current epoch and either it has not expired or its refresh has not failed since; `false` otherwise.
  An expired snapshot whose refresh is queued or running still counts, because sync keeps it current between refreshes and the refresh is due within seconds.
  Once a refresh after expiry fails, `complete` is `false` until one succeeds: the list is still served, merged with sync, but may have missed gap changes since `fetched_at`.
  A refresh that ends `too_large` or `forbidden` deletes the snapshot (decision 2), so `complete` is `false` and the list is the projection.
  `snapshot` is ADR 0126's detail object for the room's `members` row, so a client can tell "fetching" (`queued` or `running`), "too large", "forbidden", and "failing, next retry at" apart.
  As for homeserver details, a room that is not joined reads as `unknown` and is never acquired.
- **`roles` and `power_level`:** decision 4. `roles` is `"unavailable"`, and every `power_level` is `null`, when the levels cannot be resolved; a client must not substitute `users_default`.
  A read with `min_level` then fails with `409` and the error code `roles_unavailable` rather than return an empty page, so a client cannot mistake it for a room with no moderators.
- **Cost:** the read merges and folds the room's rows in memory, then filters, orders, and slices.
  Its size is bounded by `MAX_SNAPSHOT_ENTRIES` plus the room's projection, and on the surveyed instance is at most about 2,600 rows.
  A maintained read model with stored keys and a fold-rule version replaces this if measurement shows it is too slow; the API does not change.

The legacy `/members` keeps its contract, shape, and cost: the projection only, unpaginated, never scheduling acquisition.
It remains the source for sender display names and mention candidates, and its description says it is partial and points at `/members/page`.
Mention completion over the complete list is not part of this ADR.

The route is additive, per ADR 0099; `/members/page` is a static segment, so a later `/members/{user_id}` route can sit beside it.

### 4. Resolved levels come from the SDK, and `/power_levels` gains them too (issue 324)

A member's level is resolved with the SDK's `RoomPowerLevels`, which applies the room version's rules: a user in `rules.privileged_creators` has an infinite level and `is_creator: true`; anyone else has their entry in `users`, or `users_default`.
`power_level` is a JSON number, or the string `"infinite"`.
The level is resolved per read rather than stored, so a power-level change shows on the next read without touching the snapshot.

`axon-api` reaches it through a consumer-owned port with an `axon-server` adapter (ADR 0021), as it reaches cached member profiles today, so `axon-api` takes no dependency on `axon-sync` or `matrix-sdk`.
The port returns the room's `users` map, `users_default`, and the privileged creators in one local read; it never requests anything from the homeserver.
When the room is not joined or the read fails, the page is still served with `roles: "unavailable"`.

Issue 324 is settled with its third shape: `/power_levels` gains `privileged_creators` (a list of user IDs, empty before room version 12) and `own_level` (the caller's resolved level, number or `"infinite"`).
Both fields are additive.
Role names such as "admin" and "moderator" stay a client presentation choice over the resolved level.

### 5. `room_details.changed` names `members`

`RoomDetailSource` gains `members`, telling a client to re-read `/members/page`.
It is published when a snapshot commit changes the merged list, when a job that a read reported as `queued` or `running` ends (as ADR 0126 decision 5 does for its jobs), and when sync inserts or changes a `room_state` member row.
Each room's processing step still publishes at most one frame with the union of its sources.
A client re-reads the pages it holds on the frame, on reconnect, and on reopen, and debounces bursts as the TUI already does for membership events (ADR 0135).

### 6. Limits and diagnostics

In keeping with ADR 0121 decision 3, the limits are constants:

| Constant                     | Value     | Why                                                                                                                                                         |
| ---------------------------- | --------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `MAX_SNAPSHOT_ENTRIES`       | 20,000    | Entries including departed members; nearly eight times the largest surveyed room's 2,583; bounds the snapshot table per room and the read's in-memory merge |
| `MAX_MEMBERS_RESPONSE_BYTES` | 10 MiB    | Admits `MAX_SNAPSHOT_ENTRIES` at up to about 520 bytes each, so the entry cap binds first at every measured average (311 to 436 bytes)                      |
| `MEMBERS_QUEUE_CAPACITY`     | 4         | Member lists are opened one panel at a time; a few rooms' worth is enough, and the lane cannot grow into a backlog                                          |
| `MEMBERS_REQUEST_TIMEOUT`    | 30 s      | A full member list is far larger than ADR 0126's tiny bodies; to be checked against the largest surveyed room                                               |
| `MEMBERS_TTL`                | 1 h       | Sync misses joins and leaves inside timeline gaps, which large, busy rooms have most                                                                        |
| `DEFAULT_PAGE` / `MAX_PAGE`  | 100 / 500 | A screenful or two per request; well under the TUI's 2 MiB response limit                                                                                   |

`available` and `too_large` stay fresh for `MEMBERS_TTL`, `forbidden` for ADR 0126's `CONCLUSIVE_DENIAL_TTL`, and failures take its `FAILURE_BACKOFF`.

The worker logs `members` jobs like its other jobs, under `axon_sync::acquisition` with `account_id`, `room_id`, `kind`, `outcome`, `elapsed_ms`, the response size, and the member count, and never member IDs, names, or response bodies.

## Alternatives considered

- **The SDK's `Room::sync_members()`.** It reads and parses the whole body with no bound, and it races sync (matrix-rust-sdk issue 1205, noted in the vendored `receive_all_members`).
  It would also make acquisition a writer of the SDK's own state.
- **Merge by write time: a `room_state` row updated after the request was sent wins.** The first draft of this ADR.
  PostgreSQL's `now()` is the transaction's start, so a sync write that starts before the request and commits after it is stamped too early and loses; and the upsert accepts an equal `origin_server_ts`, so a replay bumps `updated_at` and could bring back a departed member.
- **Fetch without departed members (`not_membership=leave`).** 20 to 50 percent smaller, but a user missing from the snapshot has no event to compare, so the merge would fall back on clocks or accept stale rows.
- **Store folded keys in the snapshot.** Faster to order and search, but projection rows have none, and a fold-rule change would mix old and new keys in one listing; keys belong in a maintained read model with a rule version if one is ever needed.
- **Read the SDK's member cache where `are_members_synced()` holds.** Free in encrypted rooms the account has sent to, but a second source with its own freshness rules and none at all elsewhere.
- **`/joined_members`, plus `/members?membership=invite` and `?membership=ban`.** Smaller per member, since it carries no event envelope, but three or four requests that do not form one snapshot, and no event IDs or timestamps for the merge.
  On the surveyed instance the largest complete response is about 960 KB, so size does not justify it.
- **Paginate the existing `/members`.** Breaks the web client's display-name map and mentions, and an independently distributed TUI that expects a bare array.
- **Write the snapshot into `room_state`.** Makes acquisition a writer of synced state, against ADR 0111, and a snapshot cannot say which of its rows are newer than sync's.
- **Disable lazy loading in the primary sync.** Every room pays for its full member list on every cold start, including the thousands of small rooms that are already complete.
- **Fetch every incomplete room in the background.** Keeps panels warm but spends upstream requests on rooms nobody opens.
- **Order by role.** Puts the most useful members first, but the cursor would depend on power-level state that changes independently of membership; `min_level` gives the same view without it.
- **Store resolved levels in the snapshot.** Faster to filter, but stale after every power-level change until the next refresh.

## Consequences

- Both clients can list and search everyone in a room with up to `MAX_SNAPSHOT_ENTRIES` membership entries, and say plainly when the list is partial and why.
- Opening a large room's member list costs one upstream request per hour while it is read; the largest surveyed room costs about 960 KB.
  Read-triggered fetches are not yet budgeted per account ([issue 1091](https://github.com/matrix-axon/matrix-axon/issues/1091)).
- An account can have two homeserver requests in flight, one in each acquisition lane.
- Roles are honest for every room version, including v12 creators, and `/power_levels` tells a client its own level (issue 324).
- The legacy `/members` and the room-open path are unchanged.
- New schema: the `room_member_snapshots` table and the `members` kind on `room_detail_acquisitions`; forward-only migrations with no backfill.
- Public rooms much larger than any on the surveyed instance are untested; past the cap they stay partial and say so.

## Delivery

One silo per PR; every server PR updates OpenAPI, the regenerated web schema, `docs/room-metadata.md`, and `docs/client-parity.md`.

| Step | Silo   | Change                                                                                                                                              | Closes                                                         |
| ---- | ------ | --------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| 1    | server | `privileged_creators` and `own_level` on `/power_levels` (decision 4)                                                                               | [324](https://github.com/matrix-axon/matrix-axon/issues/324)   |
| 2    | server | `/members/page` over the projection alone, with paging, `q`, `min_level`, roles, `complete: false`, and the `members` frame source for sync changes | —                                                              |
| 3    | server | The `members` job, its lane, and the snapshot table, which make the list complete (decisions 1, 2, 5, 6)                                            | [659](https://github.com/matrix-axon/matrix-axon/issues/659)   |
| 4    | TUI    | `/whereami` reads `/members/page`: paging on scroll, server search, coverage, and resolved roles                                                    | [1084](https://github.com/matrix-axon/matrix-axon/issues/1084) |
| 5    | web    | Room Information's member section reads `/members/page` in its own store                                                                            | [1085](https://github.com/matrix-axon/matrix-axon/issues/1085) |

Step 2 lets both clients start before step 3 lands; their PRs can follow step 2 or step 3.

## Verification

- **Merge, against the store:** a `room_state` event with a later `origin_server_ts` beats the snapshot's and an earlier one loses; equal timestamps with different events go to the snapshot; the same event in both is one member; a user only in `room_state` is listed from it; a winning `leave` is left out of both the page and `total`; with no available snapshot the read is the projection; a new epoch removes the snapshot.
- **Merge races:** a sync write whose transaction starts before the request and commits after it, carrying a newer membership, wins; a replayed or late-delivered older join for a user the snapshot shows as departed, including one with an equal `origin_server_ts` for the projection's upsert, does not bring them back.
- **`complete`:** `true` with a fresh snapshot and with an expired one whose refresh is queued or running; `false` after a refresh fails following expiry, while the rows are still served; `false` with the projection alone after a refresh ends `too_large` or `forbidden`, which deletes the rows.
- **Paging:** the documented order; no repeats or skips across pages under concurrent unrelated changes; `total`, `q` with folded input (the TUI's test vectors), `membership`, `min_level` including `"infinite"`, `leave` rejected, and the page-size bounds.
- **Acquisition, under paused tokio time against a mock homeserver:** `200` stores every entry, departed members with their event only; an oversized body is refused before parsing, and a list over `MAX_SNAPSHOT_ENTRIES` is not stored, both recorded `too_large`; `403` records `forbidden`; failures back off; repeated reads schedule nothing until `next_attempt_at`; a leave during the request commits nothing, including when a rejoin follows; unjoined rooms make no request; `/members` never schedules a job.
- **Lanes:** a hung `members` request neither delays the room's alias, directory, or absence jobs nor evicts them from their queue; a full `members` queue drops its own oldest job.
- **Roles:** a v12 room's creator resolves to `"infinite"` with `is_creator`; a pre-v12 room uses `users` and `users_default`; an unresolvable room returns `roles: "unavailable"` and null levels, and `min_level` there returns `409 roles_unavailable`; `/power_levels` reports `privileged_creators` and `own_level`.
- **Frames:** `members` fires on a snapshot change, on the end of an observed job, and on a sync member change, and stays silent on identical rewrites.
- **Live, on a disposable Synapse stack:** a room with more members than the projection holds becomes complete after one read; a join made while sync is paused appears after the next refresh; a v12 room's creator shows as such.
- **Production, read-only:** after step 3, the largest surveyed room's `/members/page` `total` matches Synapse's non-departed member count.
