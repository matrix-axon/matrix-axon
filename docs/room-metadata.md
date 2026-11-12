# Room metadata

[ADR 0111](adr/0111-cached-progressive-room-metadata.md) records the architecture for cached room metadata and bounded progressive enrichment.
This guide documents the implemented API, current sync coverage, verification, and remaining work.
[Tracking issue 618](https://github.com/matrix-axon/matrix-axon/issues/618) records the remaining acquisition, discovery, and client work.

## Cached state details

`GET /v1/accounts/{account_id}/rooms/{room_id}/metadata` returns eight typed snapshots from that account's cached room state.
It requires the same bearer authentication as other `/v1/` reads.
It performs one database query for a fixed set of singleton tuples, with no homeserver request, membership aggregation, or discovery-cache merge.
Each state's PostgreSQL-rendered content is limited to 128 KiB before transfer and JSON decoding.
This is a local transfer budget with headroom for JSONB spacing above Matrix's compact event-size limit; it is not an upstream event-validity check.
PostgreSQL still detoasts and renders the full selected value before measuring it; this cap does not bound database-side rendering work.
[Issue 631](https://github.com/matrix-axon/matrix-axon/issues/631) tracks that remaining resource bound.
An oversized state keeps its provenance but withholds its content; other fields remain available.

| Snapshot             | State type                  | Detail fields                                                                            |
| -------------------- | --------------------------- | ---------------------------------------------------------------------------------------- |
| `aliases`            | `m.room.canonical_alias`    | `alias`, `alt_aliases`                                                                   |
| `creation`           | `m.room.create`             | `room_version`, `creator`, `additional_creators`, `federate`, `room_type`, `predecessor` |
| `join_rules`         | `m.room.join_rules`         | `join_rule`, `allow` conditions                                                          |
| `encryption`         | `m.room.encryption`         | `algorithm`, `rotation_period_ms`, `rotation_period_msgs`                                |
| `power_levels`       | `m.room.power_levels`       | Role thresholds, `users`, `events`, `notifications.room`                                 |
| `server_acl`         | `m.room.server_acl`         | `allow_ip_literals`, `allow`, `deny`                                                     |
| `history_visibility` | `m.room.history_visibility` | `history_visibility`                                                                     |
| `guest_access`       | `m.room.guest_access`       | `guest_access`                                                                           |

Creation content maps Matrix `m.federate` to API `federate` and Matrix `type` to API `room_type`.
These names deliberately differ from the stored Matrix keys.

### Known absence

A missing tuple reads as `unknown`: Axon has not been told, and nothing can be inferred ([ADR 0111](adr/0111-cached-progressive-room-metadata.md)).
Sync cannot say a room has no such state, because Sliding Sync does not guarantee complete required state.
Only the account's homeserver can: `GET /rooms/{roomId}/state/{type}/` answering `404 M_NOT_FOUND` is the specification's statement that the room has none ([ADR 0126](adr/0126-room-detail-acquisition-and-coverage.md) decision 2).

Every snapshot therefore carries two more fields, both `null` for a type with a tuple:

- `absence_confirmed_at`: Unix ms when the homeserver last confirmed the type absent, from the PostgreSQL clock; `null` without a confirmation.
- `absence_check`: the state of confirming it, the same refresh object `homeserver_details` uses (below), or `null` when no attempt has been made and none is queued.

| `status`  | `absence_confirmed_at` | `absence_check`                                     | Meaning                                                             |
| --------- | ---------------------- | --------------------------------------------------- | ------------------------------------------------------------------- |
| `unknown` | `null`                 | `null`                                              | Not attempted; nothing can be inferred                              |
| `unknown` | `null`                 | `queued` or `running`                               | A confirmation is coming                                            |
| `unknown` | `null`                 | `idle` with a `failure` or future `next_attempt_at` | The attempt failed or was denied; nothing can be inferred           |
| `unknown` | present                | any                                                 | The homeserver confirmed the room has no such state as of that time |
| any other | `null`                 | `null`                                              | A tuple exists and describes the type                               |

The `status` enum is unchanged, so existing clients keep working.
Known absence is a statement about the room's current state, not a manufactured value: `content` stays `null`.
A client may apply a Matrix default only where the specification defines what the absent state means, such as no `m.room.encryption` meaning the room is not encrypted, or no `m.room.server_acl` meaning no servers are denied.

The read itself stays local and never waits.
For a room the account has joined, when a type with no tuple has no confirmation row, an expired confirmation, or a passed backoff, the read queues one `absence` job for the room on the account's acquisition worker and reports it as `queued`.
The job confirms the room's due types in a fixed order, encryption first because a false "unencrypted" is the costliest mistake: encryption, server ACL, join rules, history visibility, guest access, power levels, canonical alias, creation.

| Answer            | Recorded                                                                                                   |
| ----------------- | ---------------------------------------------------------------------------------------------------------- |
| `404 M_NOT_FOUND` | A confirmation, valid for 24 hours                                                                         |
| `200`             | No confirmation, and the body is never read; a warning names a sync coverage gap; asked again after 1 hour |
| `403`             | No confirmation (the account may no longer be joined); not asked again for 1 hour                          |
| Anything else     | A failure with backoff (see [Acquisition](#acquisition))                                                   |

A confirmation is deleted when a tuple of its type arrives, when the account leaves the room, when the room is purged, and when the account is removed.
A confirmation and a tuple of the same type serialize on an advisory lock, so a confirmation cannot commit beside a tuple that arrived during its request; a snapshot with a tuple never reports one in any case.

Each snapshot includes `status`, `event_id`, `sender`, `origin_ts`, `redacted`, `redaction_event_id`, and `content`.
`origin_ts` is the upstream event timestamp, not the time of the last successful sync or an access-freshness guarantee.

| Status        | Meaning                                                                                              |
| ------------- | ---------------------------------------------------------------------------------------------------- |
| `unknown`     | No cached tuple; Axon cannot establish that the setting is unset. Provenance and content are `null`. |
| `available`   | Typed content is available, including explicit empty lists and omitted fields.                       |
| `unavailable` | A tuple exists with SQL NULL or JSON null content; neither establishes redaction or removal.         |
| `partial`     | Valid fields and entries remain available; `invalid_fields` identifies withheld malformed data.      |
| `invalid`     | Stored content has an incompatible shape; content is withheld.                                       |
| `too_large`   | Stored content exceeded the content-size bound; content is withheld.                                 |

Only `available` and `partial` snapshots have non-null content.
A `partial` snapshot preserves valid fields and entries and reports malformed paths in `invalid_fields` (for example, `rotation_period_msgs`, `allow[]`, or `users.*`).
Paths use wildcards instead of upstream map keys, are deduplicated, and never contain offending values.
Clients must not treat a malformed field or a filtered list/map as confirmed absent or complete, or apply defaults to an invalid field.
All other snapshots have an empty `invalid_fields` list unless typed decoding still fails after field validation.
Nullable content fields preserve omission instead of filling Matrix defaults.
In particular, an unknown encryption snapshot does not mean "unencrypted," and an unknown alias snapshot does not mean "no aliases."
For available creation state, the enclosing `sender` provides the create-event sender when a room version omits the `creator` content field.
A creation predecessor can omit `event_id`; its `room_id` remains available.
Power levels describe configured state, with legacy decimal strings and floats normalized to integers (floats truncate toward zero); they are not resolved permissions and must not drive authorization decisions.
Legacy normalization currently does not consult the room version; [issue 632](https://github.com/matrix-axon/matrix-axon/issues/632) tracks version-aware validation or diagnostics.
Unknown condition types retain their `type` and optional `room_id`; extension-specific payloads are not exposed by this typed read.
Redaction evidence is independent of content availability, consistent with [Matrix redaction semantics](https://spec.matrix.org/v1.19/client-server-api/#redactions).
`redacted: true` means the SDK supplied a redacted state form.
`redacted: null` means Axon has no positive redaction evidence, including legacy rows and unknown tuples.
Absence of the SDK marker does not prove that content is original rather than server-pruned or censored, so Axon does not emit `false`.
`redaction_event_id` identifies a known redaction when available; a null ID does not negate positive evidence.
An empty object or NULL content alone never establishes redaction.
Already-redacted state can retain room-version-protected fields and remain `available` or `partial`.
Axon uses the SDK's retained content instead of implementing its own pruning rules.
A raw timeline redaction row alone never changes the metadata's evidence or withholds its content, with one exception below for state the SDK never holds.
This also avoids withholding legitimate protected fields in pre-migration rows that already contain a redacted form.

A bounded worker reconciles SDK-redacted singleton state into the shared `room_state` projection.
It covers the eight metadata types plus name, topic, avatar, and tombstone, so `/metadata`, `/info`, room-list summaries, and space display enrichment use the same retained state.
It does not enumerate membership or space-link state keys.
Live redactions enqueue a hint only if the target event ID matches a current metadata/display singleton in the same account and room.
The filter uses fixed primary-key probes, with a two-second deadline; failure conservatively queues a hint, without treating the timeline row as evidence.
Marker-free singleton state writes check only their own SDK cache entry and enqueue a hint if that same event is already redacted there.
Ordinary message redactions and normal initial hydration therefore do not enqueue full room scans.
The state-write check closes redaction-before-state delivery races when the target filter saw no tuple yet.
These checks wait for bounded local I/O, never for the subsequent repair.
Each account has one paced worker, a 32-entry hint queue, and at most eight active rooms per tick: four hints and four rooms from a keyset-paged room-summary sweep.
Duplicate hints coalesce; draining stops at four distinct rooms, leaving the channel tail for the next tick.
Queue overflow and interrupted jobs heal through subsequent sweeps.
The worker reads twelve singleton SDK cache entries per room, bounds raw JSON before decoding to 128 KiB, and uses a two-second deadline for each room and each sweep-page read.
Guest-access and server-ACL state usually reaches Axon only through the supplementary connection, so the SDK cache may hold no copy to repair from, and that connection does not redeliver a redacted target.
When the SDK has no entry of either type, the worker makes one more bounded query for a stored `m.room.redaction` event targeting Axon's current tuple of that type (tuples up to 128 KiB, redactions up to 64 KiB, under the reconciliation deadlines).
A homeserver delivers redactions it has accepted but not applied: from room version 3 it applies one only after confirming that its sender may redact others' events or is on the target sender's server.
The worker applies a stored redaction only under that same rule, using the room's current power levels from the SDK cache; without them, only a redaction from the target sender's server applies.
It then redacts the tuple as the SDK store would: ruma's `redact` under the room-version rules of the SDK's stored room info.
The result goes through the same event-ID-guarded repair write, so a replacement is never touched by its predecessor's redaction.
Ticks run once per second with missed ticks skipped; after a complete sweep the next sweep waits five minutes.
That idle interval reduces repeated background SQLite reads; retained non-joined rooms can still occupy cheap sweep-page slots, but the worker checks SDK membership before reading singleton state.
All acquisition is local SDK cache I/O; this worker makes no homeserver requests.
One room or field failing is logged and skipped without stopping sync.
The worker is canceled and joined with its account run.

Repair updates only an existing tuple whose event ID still matches the SDK form.
It cannot overwrite a replacement, advance state from historical data, or recreate a purged room or removed account.
A repaired display tuple and its room-summary projection commit in the same transaction.
Same-event replays cannot clear positive evidence or restore original content; a new replacement event has independent evidence.
Startup sweeps repair a crash between the SDK cache commit and Axon's projection update without a cold resync or re-dispatched state event.
The nullable evidence columns are added by a forward-only migration; older rows are not guessed or rewritten in a bulk backfill.

A client should re-read when reopening the panel, on reconnect, and on a `room_details.changed` frame naming `metadata` (see [Change notification](#change-notification)).
That frame follows every change to a stored snapshot, from either sync connection and from redaction repair, so an open panel needs no polling.
Both the web Room Information panel (ADR 0133) and TUI `/whereami` (ADR 0135) consume these frames, known-absence confirmations, and the separate homeserver details.
The TUI refreshes each named source independently and re-reads all visible sources on reconnect, reopen, and explicit refresh; failed HTTP reads retry with bounded backoff while the popup is open.
Unknown account/room IDs return unknown snapshots with HTTP 200, matching existing cached state-read conventions.
Cached state from a room the account has left remains historical cached state according to the existing retention policy; this endpoint does not assert current membership or upstream access.

## Joined-room member counts

`GET /v1/accounts/{account_id}/rooms/{room_id}/info` adds nullable `member_counts`.
An observed summary contains `joined`, `invited`, and `observed_at` (Unix milliseconds).
Counts come from an atomic SDK `RoomInfo` summary snapshot, never from the lazily loaded member-list projection.
A temporary, version-pinned SDK base patch preserves whether each count was supplied and invalidates that evidence on membership transitions and `initial: true` room replacements.
See `crates/third-party/README.md` for the upstream revision, release checksum, complete patch, and removal criteria.
A pair is published only when both fields are known; an explicitly supplied zero invited count is valid, while a missing invited field remains unknown.
A server-reported zero joined count clears any previous pair because a joined room must include the account itself.
A missing SDK room, uninitialized summary, or count outside signed 64-bit storage range leaves the last persisted observation unchanged.
Positive non-joined membership evidence or a known membership transition invalidates it, including a rejoin whose new summary omits counts.
Existing rows start unknown; there is no membership backfill or new room-list aggregation.

`observed_at` is the database-clock timestamp captured before Axon read the current changed pair from its local SDK cache.
Identical observations do not rewrite the summary row or advance its timestamp.
It records when the current pair changed locally, rather than the most recent confirmation of that pair, and is not a freshness signal.
Observations and leave watermarks both use the PostgreSQL clock, so a difference between the database and application host clocks cannot delay a valid rejoin observation.
The database rejects observations older than the persisted observation or invalidation watermark.
A recent observation can still reflect a disconnected SDK's old summary.
Consumers must consider account sync health as well as the timestamp; this endpoint does not assert upstream freshness.
Counts persist across process restarts and remain readable while sync is disconnected.
A local leave atomically clears the observation; a rejoin needs both counts supplied in its new membership epoch.
A confirmed upstream `gone` verdict also withholds counts.
Retained room state after leave remains historical, while this count field becomes unknown.

Each account has one cancelable worker started before sync and before the awaited startup decryption sweep.
A slow key-backup request cannot hold up local member-count reconciliation.
The worker subscribes before startup reconciliation.
One-second ticks skip missed ticks and process at most four queued hints or retries plus four keyset-paged room-summary rows.
A coalesced queue holds at most 32 rooms and makes up to four attempts for a projection-order race or transient failure; overflow and exhausted retries fall back to the progressive sweep.
An observation superseded by an equal or newer database watermark is terminal and consumes no retry slot.
At most 32 SDK notifications are consumed per tick, coalescing duplicates within that budget and preserving the receiver tail.
Lagged notifications wake a sweep without rewinding its cursor, so continuous traffic cannot starve later rooms.
Each page and each room observation has a two-second deadline, and every room failure is logged and skipped independently.
Reconciliation transactions also set PostgreSQL-local statement and lock deadlines of 1.5 seconds and one second, so timed-out awaits cannot leave unbounded database work.
After a complete sweep, another begins after five minutes; startup begins immediately.
An offline-to-online transition sends a coalesced wake signal to resume scanning at the next paced tick, including reconnects that do not restart the account task.
Recovery time scales with the number of cached summaries and local I/O, rather than promising a fixed five-minute completion for every account.
There is no account-sized in-memory dedup map, unbounded room enumeration, or remote request.
The worker only updates existing account/room summary rows; purge or account removal cannot be undone by a late completion.
Its cancellation token and join handle are owned by the account run.
Cancellation during a page read stops either worker before it drains hints or reads SDK state.
An unexpectedly closed SDK update channel stops the member-count watcher with an account-scoped warning.
The keyset traversal is shared with singleton-state redaction repair.
Existing tracing controls suffice; warnings identify the worker, account, room where applicable, and an allowlisted database failure category or deadline without logging private metadata bodies.
Set `RUST_LOG=warn,axon_sync::room_sweep=debug` to see `local SDK reconciliation sweep started` and `local SDK reconciliation sweep completed` for each account and worker.
Completion includes the number of room-summary rows visited, elapsed seconds, and whether another pass was requested during traversal.
Completion means every page was traversed; rooms whose SDK counts remain unknown can still have no observation.

Each stored or cleared pair publishes a `room_details.changed` frame naming `member_counts`; clients re-read `/info` on that frame, on reconnect, and on reopen.
A local leave clears counts in the database without the worker, so it publishes no `member_counts` frame; clients learn of the leave from its own membership events.
Server counts complete [issue 620](https://github.com/matrix-axon/matrix-axon/issues/620).
The TUI's `/whereami` (ADR 0114) and the web client's Room Information Members row ([ADR 0133](adr/0133-web-room-information-metadata-and-discovery.md)) show them; both show "unknown (not yet observed)" for a null pair, never zero.

Two maintenance improvements remain deferred: sharing each account's traversal between the count and redaction workers to reduce database round trips, and automatically checking that the documented SDK patch reproduces the vendored source from the pinned release archive.
The workers currently share traversal code but keep independent cursors and deadlines.

## Calculated room display names

`GET /v1/rooms` adds nullable `display_name` to each room ([ADR 0118](adr/0118-room-display-names-from-the-sdk.md)).
It is the name the Matrix spec's naming algorithm gives the room: `m.room.name`, else the canonical alias, else a name built from up to five other members ("Alice, Bob", "Alice, Bob, and 3 others", "Empty Room (was Alice)", "Empty Room").
matrix-rust-sdk computes it on each sync response that touches the room, excluding service members listed in `io.element.functional_members`; Axon copies the SDK's cached value and never derives it from the lazily loaded member list.
The member-count worker above writes it in the same per-room step, so it shares that worker's hints, retry queue and paced sweep.
The counts run first and the display name second, each under its own two-second deadline, and the retry queue records which of the two failed and reruns only that one.
Only joined rooms are written, values are cut to 512 characters, and an unchanged value is not rewritten.
A room whose summary row does not exist yet takes a bounded retry and is picked up by a later sweep.
Existing rows start `null` after the upgrade and fill as the first sweep reaches them.
A summary rebuild, including the per-room rebuild a purge runs, keeps the column: the value cannot be re-derived from stored events, and no SDK update follows a rebuild to rewrite it.

Each applied change publishes a `room_display_name.changed` WebSocket frame with `room_id` and `display_name`.
That includes the first fill after the upgrade, one frame per room at the sweep's pace of a few rooms per second, so a client that loaded the list while the column was `null` picks up names without reloading.
Clients should title a room `name`, then `canonical_alias`, then `display_name`, then its room ID: the state fields change as soon as their events arrive, while the calculated name follows the reconciler.
Warnings carry `source = "sdk_display_name"`; set `RUST_LOG=warn,axon_sync::display_names=trace` to see each stored value's account and room, never the name itself.

## Space children

`GET /v1/accounts/{account_id}/rooms/{room_id}/space/hierarchy` pages through a space's children ([ADR 0128](adr/0128-paged-space-hierarchy-and-unjoined-room-discovery.md) decision 1).
It reads only local data and never waits for the homeserver.

| Query    | Meaning                                                                                       |
| -------- | --------------------------------------------------------------------------------------------- |
| `limit`  | Page size; default 100, maximum 500, out-of-range values clamped                              |
| `cursor` | The previous page's `next_cursor`; bound to the account and space it was issued for, else 400 |
| `enrich` | `true` asks Axon to discover unjoined children's summaries in the background (see below)      |

The response is `{ room_id, children, next_cursor, total_children, discovery }`.
`next_cursor` is `null` only on the last page, so a space of any size reads to the end, and `total_children` counts every current link in the same snapshot as the page.
A child counts only when its `m.space.child` content has a non-empty `via` list of server names, as the Matrix spec requires; this is also how a removed link (empty content) drops out.
Child ids longer than the spec's 255-byte room-id limit are left out.

Children are in MSC1772 order: `order` ascending by code point with absent last, then the link's `origin_ts`, then child room id by code point.
An `order` that is not a string of at most 50 printable ASCII characters counts as absent, and the row's `order` field reads `null`.
Each page starts strictly after the last row of the previous one, so links added or removed between pages never duplicate or skip another link.
A link whose `order` or `origin_ts` changes between pages moves in the sort and can appear twice or not at all in one walk; clients de-duplicate by room id.

Each child carries the `space/children` fields (`room_id`, `via`, `order`, `suggested`, and the child's cached `name`, `avatar_url`, and `room_type`), plus `parent_room_id` (always the requested space for now), `joined`, whether the account's own membership in the child is `join`, and `discovered` (below).
Set `RUST_LOG=axon_api::routes::rooms=debug` to log each page's account, space, limit, returned rows, total, and whether it scheduled a crawl.

### Unjoined-room discovery

Unjoined children usually have no synced state, so their synced fields are `null`.
Discovery fills a separate summary for them from the account's homeserver ([ADR 0128](adr/0128-paged-space-hierarchy-and-unjoined-room-discovery.md) decisions 2-5): a crawl of the space's hierarchy fills every child, and a room summary fills one child a user selected (see [Room summaries](#room-summaries)).
Each child's `discovered` is that summary, or `null` when Axon has none; it is always `null` for a joined child, whose synced state is authoritative.

```json
{
  "status": "available",
  "source": "hierarchy",
  "observed_at": 1762500000000,
  "expires_at": 1762521600000,
  "withheld": [],
  "name": "General",
  "topic": "Say hello",
  "avatar_url": "mxc://example.org/abc",
  "canonical_alias": "#general:example.org",
  "num_joined_members": 42,
  "join_rule": "public",
  "room_type": null,
  "world_readable": false,
  "guest_can_join": false,
  "encryption": null,
  "room_version": "11"
}
```

Show a child's synced fields when it is joined, its `discovered` fields when it is not and `discovered.status` is `available`, and otherwise the synced fields, then its room id.
Axon never merges the two, so a client can always tell discovered data from synced state (ADR 0111).

| `status`       | Meaning                                                                                                          |
| -------------- | ---------------------------------------------------------------------------------------------------------------- |
| `available`    | The summary fields are the homeserver's latest answer                                                            |
| `not_returned` | A complete crawl of a linking space no longer returned the room; the account has probably lost access. No fields |
| `forbidden`    | The homeserver refused a summary of the room (`403` to a room summary). No fields                                |
| `not_found`    | The homeserver does not know the room, or the account cannot see it (`404` to a room summary). No fields         |

A summary field is `null` when the latest answer did not carry it, or when it failed validation, in which case `withheld` names it: a name over 1 KiB, a topic over 8 KiB, an alias that does not parse or an avatar that is not an `mxc://` URI, a member count that is not a non-negative integer, a string containing U+0000 (which PostgreSQL cannot store), or any value of the wrong type.
A field a newer answer drops is removed, not kept from an older one.
`encryption: null` means unknown, never unencrypted: homeservers before Matrix v1.15 do not report it.
`source` is `hierarchy` for a crawl's answer and `summary` for a room summary's.
`observed_at` and `expires_at` are Unix ms from the PostgreSQL clock; a hierarchy summary is fresh for six hours, a room summary for one hour, and a `forbidden` or `not_found` answer for one hour.
One row per account and room is shared by every space that links the room, so the latest conclusive answer wins.

**When Axon asks.**
A read with `enrich=true` schedules a crawl of the space only when the account has joined the space, the space links at least one child the account has not joined, and its crawl is due: there is none, the last one ended (`complete` or `truncated`) more than six hours ago, or an unfinished one (`in_progress`, or `failed` past its `next_attempt_at`) has no page queued or running.
Every other read is pure, including room-list grouping and any read of a space the account has not joined, so polling cannot drive upstream traffic.
Spaces are never crawled in the background.

**The crawl.**
A crawl is a run of `GET /_matrix/client/v1/rooms/{space}/hierarchy` pages with `max_depth=1` and `suggested_only=false`, one per job on the account's acquisition worker (see [Acquisition](#acquisition)).
After each page commits, the next page queues behind whatever is already waiting, so a panel opened during a long crawl is served between pages.
A returned room is cached only when the space links it with a current `m.space.child` and the account has not joined it; any other room in the response is dropped, so local state alone decides membership, order, and `suggested`.
The space's own entry, `children_state`, and the response's `membership` are ignored.
A local link the homeserver did not return stays listed with its synced fields.

`discovery` reports the space's crawl on every read, with or without `enrich`, so it is the source of truth after a reconnect:

| Field             | Meaning                                                                                                                                                   |
| ----------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `state`           | `idle`, `queued`, or `running`, the worker's activity for the crawl; in memory only, so `idle` after a restart                                            |
| `outcome`         | `null` before the first crawl; `in_progress`, `complete`, `truncated`, or `failed`                                                                        |
| `pages`           | Pages the current crawl has committed                                                                                                                     |
| `page_limit`      | Rooms asked for per page: 50, halved for the rest of a crawl after a page over 2 MiB                                                                      |
| `rooms_cached`    | Children the current crawl has returned so far                                                                                                            |
| `started_at`      | When the current crawl started from its first page                                                                                                        |
| `completed_at`    | When it ended `complete` or `truncated`                                                                                                                   |
| `expires_at`      | Until when an ended crawl, a denial, or `unsupported` stands                                                                                              |
| `support`         | `unknown`, `supported`, or `unsupported` (`404 M_UNRECOGNIZED`, a bare `404`, or `405`; retried after 24 hours)                                           |
| `failure`         | The last failure, or why a `truncated` crawl stopped: an ADR 0126 category, `forbidden`, `not_found`, `too_large`, `pagination_loop`, or `cursor_expired` |
| `next_attempt_at` | When a failed crawl may resume                                                                                                                            |

- **Complete:** a page with no `next_batch` ends the crawl `complete`.
  In the same transaction, every linked child the crawl did not return, whose summary is older than the crawl, becomes `not_returned`, and every discovery row that no joined space links any more is deleted.
  If the crawl skipped an entry with no valid room id, it marks nothing `not_returned`, because the skipped entry may have been one of them.
- **Truncated:** after 40 pages (2,000 children), when a `next_batch` repeats a token already used (`pagination_loop`), or when a single room's page is still over 2 MiB at a page limit of one (`too_large`).
  A truncated crawl marks nothing `not_returned`, and like a complete one it stands for six hours.
- **Failed:** a transient failure keeps the committed pages and the cursor and takes ADR 0126's backoff; the next `enrich` read after `next_attempt_at` resumes from the cursor.
  `400 M_INVALID_PARAM` on a resumed page means Synapse's 5-minute pagination session expired: the crawl restarts from the first page once, and a second time fails with `cursor_expired`.
  `403` and `404 M_NOT_FOUND` are `forbidden` and `not_found`, retried after an hour; cached summaries stay until a later complete crawl or a leave removes them.

**Transitions.**
Joining a discovered room deletes its row with the member event, and leaving a joined room deletes any row a racing commit left.
Leaving a space deletes its crawl and every discovery row no other joined space links, and starts a new membership epoch, which discards any page in flight; each page commits under a share lock on the space's summary row, so a leave waits for a commit and then removes what it wrote.
A purge, a summary rebuild, and account removal delete them too.
A room's room-summary refresh row goes wherever its discovery row goes.
Values, cursors, page limits, expirations, and backoff persist in `room_discovery`, `room_discovery_refresh`, `space_discovery`, and `space_discovery_children`.

#### Room summaries

`GET /v1/accounts/{account_id}/rooms/{room_id}/discovery` returns what Axon knows about one unjoined room, such as a space child a user selected in Room Information (ADR 0128 decision 5):

```json
{
  "room_id": "!child:example.org",
  "eligible": true,
  "discovered": {
    "status": "available",
    "source": "summary",
    "name": "General",
    "…": "…"
  },
  "support": "supported",
  "refresh": { "state": "idle", "failure": null, "next_attempt_at": null }
}
```

`discovered` is the same shared row a crawl writes and `space/hierarchy` returns, so a summary refreshed here shows in the space's children too, and a later crawl may replace it.
It is always `null` for a joined room.
The read returns at once and never waits for the homeserver.

| Field      | Meaning                                                                                                                                                                                                       |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `eligible` | The account has not joined the room, and a space the account has joined links it with a current `m.space.child`. No other room is ever requested, so the read cannot probe arbitrary room ids                 |
| `support`  | `unknown`, `supported`, or `unsupported`: the homeserver advertises neither Matrix v1.15 nor `im.nheko.summary`, or answered `404 M_UNRECOGNIZED`, a bare `404`, or `405`; checked again after 24 hours       |
| `refresh`  | ADR 0126's refresh object: `state` is the worker's activity while a summary is due and `idle` otherwise; `failure` is the last ADR 0126 category; `next_attempt_at` ends a backoff or an `unsupported` answer |

**When Axon asks.**
A read of an eligible room schedules one summary request when the room has no discovery row or its row has expired, no backoff or `unsupported` answer is in force, and no request is already queued or running.
A fresh row from either source is served as it is, so reopening a room within an hour, or within six hours of a crawl that returned it, costs nothing upstream.
Summaries are never requested for several children at once; a crawl serves that need.

**The request.**
The worker reads the account's `/versions` from the SDK's cache and sends `GET /_matrix/client/v1/room_summary/{roomId}` when the homeserver advertises v1.15 or later, MSC3266's `GET /_matrix/client/unstable/im.nheko.summary/rooms/{roomId}/summary` when it advertises only `im.nheko.summary`, and nothing otherwise. It never fetches `/versions` itself: an empty cache, as just after a first start, is a `transport` failure with backoff.
`via` lists at most three distinct servers from the room's current links in joined spaces, in space id order and then link order, skipping any that is not a valid server name.
It takes a gate permit like every acquisition request, with a 10-second timeout and the 64 KiB body limit.
A `200` must describe the requested room; its fields are validated exactly like a hierarchy entry's, and its `membership` is ignored because local membership is authoritative.
`403` and `404 M_NOT_FOUND` replace the row with `forbidden` or `not_found` and no fields; any other failure keeps the last answer and `support` and backs off as ADR 0126 does, so a room that was `unsupported` keeps its crawl fallback through the backoff.

**Fallback.**
While `support` is `unsupported`, the read schedules a crawl of the first joined space, by room id, that links the room and whose crawl is due, exactly as a `space/hierarchy` read with `enrich=true` would.
The room's summary then arrives with that crawl, announced by a frame for the space naming `space_hierarchy`.

**Fencing.**
A result commits only while the account is active, has not joined the room, and a joined space still links it, under share locks on every linking space's summary row; a leave of the last linking space waits for the commit and then removes what it wrote.
A join committed before the result wins, and joining the room deletes its row and refresh state with the member event.

The older `GET …/space/children` is unchanged: a bare array capped at 1,000 children, with no way to tell a truncated response from a complete one.
Because it is unchanged, it still returns the link's raw `order` string, so the two routes can report a different `order` for the same link when MSC1772 treats that `order` as invalid.
It stays for existing clients and will be marked deprecated once the web client, the TUI, and the oversized-space work ([issue 553](https://github.com/matrix-axon/matrix-axon/issues/553)) have moved to `space/hierarchy`.
The web client's Room Information panel has ([ADR 0133](adr/0133-web-room-information-metadata-and-discovery.md) decision 6); its spaces store, which feeds the space rail and room-list grouping, is the web client's last caller ([issue 1054](https://github.com/matrix-axon/matrix-axon/issues/1054)).

## Homeserver details

`GET /v1/accounts/{account_id}/rooms/{room_id}/homeserver_details` returns two details only the account's homeserver knows, from Axon's cache ([ADR 0126](adr/0126-room-detail-acquisition-and-coverage.md) decision 3):

- `local_aliases`: the aliases the homeserver maintains for the room ([`GET /rooms/{roomId}/aliases`](https://spec.matrix.org/v1.19/client-server-api/#get_matrixclientv3roomsroomidaliases)), as `aliases`.
  These are distinct from the advertised aliases in `/metadata` (see [Alias scope](#alias-scope)).
- `directory_visibility`: whether its directory publishes the room ([`GET /directory/list/room/{roomId}`](https://spec.matrix.org/v1.19/client-server-api/#get_matrixclientv3directorylistroomroomid)), as `visibility`: `public` or `private`.

```json
{
  "local_aliases": {
    "status": "available",
    "aliases": ["#room:example.org"],
    "fetched_at": 1762200000000,
    "expires_at": 1762200900000,
    "refresh": { "state": "idle", "failure": null, "next_attempt_at": null }
  },
  "directory_visibility": {
    "status": "unknown",
    "visibility": null,
    "fetched_at": null,
    "expires_at": null,
    "refresh": { "state": "queued", "failure": null, "next_attempt_at": null }
  }
}
```

Each detail keeps the last conclusive answer separate from the state of attempts to refresh it, so a failed refresh leaves the stale value visible.

| `status`      | Meaning                                                         | Fresh for  |
| ------------- | --------------------------------------------------------------- | ---------- |
| `unknown`     | Never answered, or the room is not eligible                     | —          |
| `available`   | The value is in `aliases` or `visibility`                       | 15 minutes |
| `partial`     | Aliases that do not parse as room aliases were withheld         | 15 minutes |
| `too_large`   | More than 256 aliases, or a body over 64 KiB; withheld, not cut | 15 minutes |
| `forbidden`   | `403`; a lost permission replaces a cached list                 | 1 hour     |
| `not_found`   | `404 M_NOT_FOUND`                                               | 1 hour     |
| `unsupported` | `404 M_UNRECOGNIZED`, a `404` with no `errcode`, or any `405`   | 24 hours   |

`fetched_at` and `expires_at` are Unix ms from the PostgreSQL clock; an `expires_at` in the past means stale.
`refresh` is the refresh object `/metadata`'s `absence_check` also uses:

- `state`: `idle`, `queued`, or `running`. A job is `running` only while its request is in flight; while it waits for the pacing interval or a homeserver gate permit it is `queued`; the gate's own state is in `GET /v1/status` (ADR 0122), and waiting is never a failure. Queue membership lives in memory only, so after a restart every refresh is `idle`.
- `failure`: the last attempt's category, `null` after a conclusive answer: `timeout`, `server_error` (5xx), `rate_limited` (429), `transport`, `unauthorized` (401), or `invalid_response` (any other status, or a body of the wrong shape).
- `next_attempt_at`: Unix ms before which no attempt is made, while a backoff is in force.

The read schedules a refresh for a detail that is unknown or expired once any backoff has passed, and is otherwise a pure read, so repeated reads and polling cannot amplify upstream traffic beyond the expiration and backoff schedule.
Only rooms the account has joined are acquired; any other room, including retained rooms after a leave and unknown room IDs, reads as `unknown` with no request, so neither endpoint can probe arbitrary rooms through the homeserver.
A change made elsewhere to a room's aliases or publication is not seen until the cached answer expires, so 15 minutes is the staleness bound.

## Acquisition

Absence confirmation and homeserver details share one acquisition worker per account run, canceled and joined with the run like the member-count worker ([ADR 0126](adr/0126-room-detail-acquisition-and-coverage.md) decision 4).
The API reaches it through a consumer-owned port; it never waits for it.

- **Queue:** at most 32 jobs per account, keyed by room and kind (`local_aliases`, `directory_visibility`, `absence`, `hierarchy`, `summary`), so a duplicate request coalesces, including with the running job.
  When the queue is full the oldest job is dropped: the newest request is the panel a user is looking at now, and the next read of the dropped room asks again.
  A dropped job that a read reported as `queued` publishes its `room_details.changed` frame, so that panel re-reads rather than showing `queued` until it is reopened.
- **Pacing:** one request at a time per account, at least 250 ms apart counted from when each request was sent, so one account's large room list cannot crowd out another account's work, and a room with every type absent fills in about 2.5 seconds.
- **Gate:** each request takes a `HomeserverGate` permit as background work (ADR 0122).
  Waiting for the permit is not part of the request's 10-second timeout (30 seconds for a hierarchy page, which can wait on federation for several remote children), and a timed-out request keeps its gate slot, which is what stops a slow homeserver from accumulating abandoned work.
- **Request:** sent with reqwest and the account's current access token rather than the SDK's `send`, without redirects.
  A successful body is read through the same bounded reader as Matrix OAuth responses and refused past 64 KiB (2 MiB for a hierarchy page) before parsing; a successful absence check is discarded unread.
  An error body of any request is read under a 4 KiB cap, for its `errcode` and `retry_after_ms` only.
  A `401` is a failure to back off from: token refresh stays with the account's sync.
- **Backoff:** one minute after the first consecutive failure, doubling to 30 minutes, and never sooner than a `Retry-After` header (delta-seconds or an HTTP date) or `retry_after_ms`, capped at 24 hours.
  A job that could not read or record the cache stores no backoff, so the worker holds it back in memory for one minute instead.
  An absence job stops at its first failed request: the failed type backs off, and the remaining types stay due for the next read, so a homeserver that is rate limiting or timing out gets one request per job, not one per missing type.
- **Fencing:** `room_summaries.membership_epoch` takes a fresh value from a sequence on every leave and rejoin, and whenever the account's own `m.room.member` event changes, which covers a kick and rejoin that sync delivers as one new join with no leave. That advance commits in the same transaction as the event.
  A job records the epoch before its request and commits only if the account is still active and joined in that same epoch, under a share lock on the summary row for the commit alone.
  A leave during a request therefore rejects its result even if a rejoin follows before it completes.
  An absence confirmation also commits only while no tuple of its type exists.
  A sequence rather than a counter matters because a purge rebuild deletes and re-inserts summary rows; a restarted counter could hand a rejoined room the epoch an in-flight job recorded.
- **Restart:** values, expirations, and backoff persist in `room_detail_acquisitions`; queue membership does not.
- **Cleanup:** rows are deleted whenever a new membership epoch starts (so on leave, and on a rejoin whose leave sync never delivered), on purge and on account removal (through the `room_summaries` foreign key), and, for a confirmation, when a tuple of its type arrives.
  The table is bounded by the account's joined rooms times ten kinds.

Set `RUST_LOG=axon_sync::acquisition=debug` to log each request with `account_id`, `room_id`, `kind`, `state_type`, `outcome`, `elapsed_ms`, and queue depth.
A hierarchy page also logs its page number, page limit, returned, cached, `unlinked`, joined, and `unidentified` counts, the names of withheld fields, `not_returned` and orphan counts, and restarts.
A room summary logs the endpoint it chose, its outcome (or `fenced`), the number of `via` servers, and the names of withheld fields.
Repeated failures, `200` absence checks (a sync coverage gap), and crawls that stop at `pagination_loop` or `too_large` log at warn; a page asked again at a smaller limit logs at info.
No log line contains alias values, names, topics, state content, response bodies, pagination tokens, or access tokens; outcomes and failure categories are allowlisted.

## Change notification

A `room_details.changed` WebSocket frame tells a client which of a room's reads to repeat ([ADR 0126](adr/0126-room-detail-acquisition-and-coverage.md) decision 5).

```json
{
  "type": "room_details.changed",
  "account_id": "…",
  "payload": {
    "room_id": "!room:example.org",
    "sources": ["metadata", "member_counts", "homeserver_details"]
  }
}
```

| Source               | Re-read                    | Published when                                                                                                                                                                   |
| -------------------- | -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `metadata`           | `GET …/metadata`           | A tuple of one of the eight types is inserted, replaced, or changes its content or redaction evidence; or an absence job recorded or removed a confirmation or changed a failure |
| `member_counts`      | `GET …/info`               | The member-count worker stores a different pair or clears one                                                                                                                    |
| `homeserver_details` | `GET …/homeserver_details` | A detail's value, freshness, or failure changed                                                                                                                                  |
| `space_hierarchy`    | `GET …/space/hierarchy`    | The frame's room is a space whose crawl committed a page that changed a cached child summary, or ended (`complete`, `truncated`, a failure, or `unsupported`)                    |
| `discovery`          | `GET …/discovery`          | The frame's room is an unjoined room whose room-summary request changed its discovery row, the endpoint's `support`, or its failure category                                     |

An acquisition job also publishes its source when it ends if a read reported it as `queued` or `running`, so a panel showing a refresh in progress learns it finished even when nothing changed.
A job dropped from a full queue publishes its source only if a read had seen it; the panel's next read asks again.
A crawl's later pages carry the first page's observation, so a client showing a crawl hears when a later page is dropped too, and its next `enrich` read resumes the crawl from its cursor.
A page that only re-observes identical summaries publishes nothing until the crawl ends.
`sources` is never empty and lists sources in the order above; clients must ignore values they do not know.
The frame is a signal, not a payload: a client never applies a diff from it.
The store reports whether each write changed the stored row, so identical rewrites, redeliveries after a restart, and writes the freshness guard refuses publish nothing.
Two concurrent writes of the same tuple can both report a change; that costs one extra re-read.

Each account has one coalescer.
The primary sync hands state to Axon one event at a time with no end-of-room signal, so producers record changes and the coalescer publishes one frame per room, with the union of its sources, once the account has had no change for 100 ms.
Under continuous changes it publishes no later than one second after the first pending change, and at once when 256 rooms are pending, so neither a busy account nor a large first sync can grow the queue without bound.
A room whose changes straddle a flush receives two frames.
The coalescer is canceled and joined with the account run, after the workers that feed it, and publishes what is pending when it stops.
SDK event handlers keep their context after the run stops, so a change they record later is published at once rather than held.
The bus is lossy like every other frame, so a lagged or reconnecting client re-reads.

After the upgrade that added the frame, the first sweep of the supplementary connection publishes one frame per room that has guest-access or server-ACL state, at the pace of its 100-room range growth.

## Alias scope

Advertised aliases are the canonical `alias` and `alt_aliases` in `m.room.canonical_alias`.
These can be read locally when that state tuple is cached.
They are not a complete federation-wide alias enumeration.
The separate Matrix `/rooms/{roomId}/aliases` endpoint lists aliases maintained by the account's homeserver; Axon serves its cached answer as `local_aliases` in [Homeserver details](#homeserver-details), never merged with the advertised aliases.
Client follow-ups should use "Advertised aliases" and "Aliases on your homeserver" rather than "Full alias list."
See the [Matrix alias API](https://spec.matrix.org/v1.19/client-server-api/#get_matrixclientv3roomsroomidaliases).

## Sync coverage and remaining acquisition

Axon uses matrix-sdk-ui 0.19.1's `SyncService` room-list required-state defaults.
Those request canonical-alias, creation, join-rule, encryption, power-level, and history-visibility state, including their full content.
The defaults do not request guest-access or server-ACL state.
Axon's supplementary state-only connection (`axon-state`, the [custom emoji](custom-emoji.md) transport from ADR 0124) requests both for every non-invite room, in its existing 100-room pages, and delivers later changes live.
It feeds the same state projection, so these two snapshots fill after its first sweep and change like the others.
What either connection delivers is positive evidence only: a type it does not deliver stays `unknown`, because Sliding Sync does not guarantee complete required state.
Even a requested state type can be unknown before room hydration or if no tuple was supplied.
The API deliberately does not hide these gaps with default values or perform a remote fetch on every detail read.
Instead the homeserver confirms absence in the background (see [Known absence](#known-absence)), and a `200` from that check is logged as a coverage gap.

Authoritative member counts are cached separately as described above; the lazily loaded member list is not an authoritative count.
Unjoined-room discovery ([issue 622](https://github.com/matrix-axon/matrix-axon/issues/622)) is in place: the paged read, hierarchy crawls, and room summaries of a selected room (see [Unjoined-room discovery](#unjoined-room-discovery)).
The discovery cache, progressive updates, and acquisition budgets follow ADR 0111 and are not implemented by the cached state endpoint.
The web client's Room Information panel reads `/metadata`, `/homeserver_details`, and member counts, and re-reads each on the `room_details.changed` source that names it ([ADR 0133](adr/0133-web-room-information-metadata-and-discovery.md) decisions 1–5).
For a space it pages the children from `space/hierarchy`, sending `enrich=true` only on the read that opening the panel makes for a joined space (plus one read to resume a paused crawl and a timer read after a transient failure), and its join dialog and Matrix-link prompt preview one unjoined room from `/discovery` (decisions 6–8).
The web spaces store still reads `space/children` ([issue 1054](https://github.com/matrix-axon/matrix-axon/issues/1054)).
TUI consumption is [issue 624](https://github.com/matrix-axon/matrix-axon/issues/624).

## Verification

The HTTP integration tests seed real PostgreSQL state and exercise the authenticated router without an upstream service.
The regression suite covers already-redacted state, filtered redaction hints, queued bursts spanning multiple ticks, stale SDK event IDs, raw redaction rows without applied evidence, both log/state arrival orders, account/room isolation, replacement events, ordered and concurrent replay protection, shared display projection repair, and evidence persistence after reopening the store.
A PostgreSQL-gated SDK-cache test applies a redaction in the real SDK state store, invokes the actual reconciliation consumer without re-dispatching a state event, and verifies retained fields and purge safety.
Use a throwaway database; these tests run migrations and write fixture accounts.

```sh
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-api --test http room_metadata -- --ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-store --test state -- --ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-sync --lib state_redaction -- --include-ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-store --test member_counts -- --ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-sync --lib member_counts -- --include-ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-sync --lib display_names -- --include-ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-sync --lib supplementary_state -- --include-ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-store --test state room_state_upsert_reports_only_real_changes -- --ignored
cargo test -p axon-sync --lib room_details
cargo test -p axon-api --lib room_details_changed
cargo test -p axon-api --lib room_details
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-store --test room_detail_acquisition -- --ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-sync --lib acquisition -- --include-ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-store --test space_discovery -- --ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-api --test http room_details -- --ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-api --test http room_state_read_endpoints -- --ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-api --test http space_hierarchy_endpoint -- --ignored
DATABASE_URL=postgres://axon:axon@127.0.0.1:5432/axon_test cargo test -p axon-store --test spaces -- --ignored
cargo test -p axon-api --lib cursor
cargo test -p axon-sync --lib room_sweep
cargo test -p axon-api --test openapi
```

For a running instance, call the new endpoint for a joined room with advertised alternative aliases and compare its `aliases.content` with the room's Matrix state.
Replace the alias state with a newer event that removes the canonical alias and empties `alt_aliases`; the next read must contain the replacement event ID and empty list.
Query a different account or unknown room and verify that missing tuples remain `unknown`.
Do not paste bearer tokens, raw event bodies, or private room identifiers into verification logs.

### Disposable live acceptance

Use an isolated Synapse/PostgreSQL/Axon stack with disposable Matrix accounts and persistent SDK storage.
Do not point these tests at an operator database or use production credentials.

1. Create rooms using room versions 10 and 11, and set power levels with distinct `ban` and `invite` thresholds and restricted join rules with an `allow` condition.
2. Wait for `/metadata` to report the state event IDs, then redact each power-level and join-rule event through Matrix.
   Poll until Axon reports positive evidence and the matching redaction ID.
   The `ban` threshold and restricted join conditions must survive; `invite` is removed in version 10 and retained in version 11.
   Check that `/info` uses the same retained join rule.
3. Redact the current room-name event and check that `/v1/rooms` clears its name after reconciliation.
4. Replace the power-level state, then redact its predecessor again.
   The replacement must keep its own event ID, content, and unknown evidence.
5. In the disposable database only, restore the original power-level content and clear its evidence while preserving the current event ID.
   The paced sweep must restore the SDK-pruned content without another state write.
   Repeat with Axon stopped, then restart against the same SDK store and verify startup recovery.
6. Redact power-level state before a second account joins the room.
   Its first hydrated snapshot must preserve protected fields and report positive evidence.

7. With `RUST_LOG=axon_sync::acquisition=debug`, read `/metadata` for a room with no server ACL and no guest access.
   Both snapshots must report `absence_check.state` `queued`, then, after one `room_details.changed` naming `metadata`, a non-null `absence_confirmed_at`; the log must show one request per absent type, encryption first.
   Set a server ACL and guest access; both snapshots must turn `available` with `null` absence fields.
8. Read `/homeserver_details`, add a local alias and publish the room in the directory through Matrix, wait out the 15-minute expiration (or expire the rows in the disposable database), and read again: after a frame naming `homeserver_details`, the new alias and `public` appear.
   Remove both and repeat.
9. Leave the room while a request is held at a slow homeserver: no row may survive, and the rejoined room must start with no confirmation or details.
10. Join a space of about 300 children, some unjoined and some private, and read its `space/hierarchy` with `enrich=true`.
    It lists every child at once; `discovery.state` turns `queued`, then `running`, and each `room_details.changed` naming `space_hierarchy` brings more `discovered` names until `outcome` is `complete` after seven pages.
    Make one discovered room private to the account (or ban the account from it), expire the crawl in the disposable database, and read with `enrich=true` again: that child's `discovered.status` turns `not_returned`.
    Leave the space: its crawl and every discovered row it alone linked are gone.
11. Rejoin the space and read `GET …/discovery` for one unjoined child without crawling: `refresh.state` is `queued`, and after a frame naming `discovery`, `discovered.source` is `summary` with the child's name; the log shows one `room_summary` request whose `via` matches the link.
    Read again within the hour: nothing is requested.
    Read the joined space itself, and a room no joined space links: both are `eligible: false` and nothing is requested.

The fixture homeserver may reject creation-event redactions; verify that a rejected request leaves Axon's evidence unchanged rather than treating the attempt as an applied redaction.
These small-room checks validate reconciliation and recovery, not throughput for hundreds of rooms.
