# ADR 0135 — TUI room details and discovery

**Status:** Accepted; joined-room detail consumption is implemented in the first PR for [issue 624](https://github.com/matrix-axon/matrix-axon/issues/624).
Space pagination and discovery browsing are subsequent TUI PRs.

## Context

[ADR 0114](0114-tui-room-information-from-cached-metadata.md) introduced typed cached metadata in `/whereami`, with polling until server notifications existed.
[ADR 0126](0126-room-detail-acquisition-and-coverage.md) now supplies known-absence confirmations, homeserver details, and `room_details.changed`.
[ADR 0128](0128-paged-space-hierarchy-and-unjoined-room-discovery.md) supplies bounded hierarchy discovery and selected unjoined-room summaries.
The TUI can consume these independently of the web implementation in ADR 0133.

## Decision

### Independent joined-room reads

The popup opens immediately from its selected room summary.
Five independent tasks read `/metadata`, `/info`, `/homeserver_details`, `/upgrade`, and the bounded `/members` projection through Axon.
Each uses the existing 30-second HTTP timeout; metadata and members retain their 2 MiB body limits, homeserver details use 128 KiB, and info and upgrade retain 64 KiB.
No network work runs in input or drawing code.

The one-room cache carries its account and room key.
Each source has its own request generation, abort handle, invalidation flag, and retry schedule.
A running source retains one pending invalidation and starts at most one follow-up when it settles.
A key change or popup close cancels all reads and retries; unmatched or canceled outcomes cannot apply.
A response for the cached room still settles if selection temporarily moves away before the tick replaces that cache; it can launch follow-up reads only when that room is selected again.
Last values and errors stay visible until that source answers again.
A same-room refresh preserves the local member filter and scroll position.

### Freshness comes from persisted-state notifications

| Signal | Read |
| --- | --- |
| `room_details.changed` for this account and room, naming `metadata` | `/metadata` |
| The frame names `member_counts` | `/info` |
| The frame names `homeserver_details` | `/homeserver_details` |
| Reopen, reconnect, or explicit refresh | All five sources |
| `m.room.member` for the room | `/members`, after a two-second pause, no later than ten seconds into a burst |
| `m.room.tombstone` or `m.room.create` | `/upgrade` |
| `m.room.redaction` | `/upgrade` immediately and `/members` through the same debounce |

Ignore unknown source names.
Metadata timeline events do not duplicate the metadata notification.
Successful sources do not poll.
An HTTP transport, timeout, size, status other than 404, or decoding failure retries only that source after 30 seconds, doubling to four minutes.
A notification cannot bypass failure backoff.
Each source permits one early reconnect retry until its scheduled retry runs; reopening or explicit refresh retries immediately.
Closing disables pending retry work while retaining the failure count and deadline; reopening bypasses the deadline without forgetting the failure count.
A different cached room starts with a fresh failure history.
An endpoint 404 is displayed as unavailable from this Axon server and does not schedule automatic retries.
On servers without notifications, snapshots still refresh on reopen, reconnect, and explicit refresh; the TUI does not infer notification support from a quiet socket.

### State labels preserve uncertainty and scope

`absence_confirmed_at` distinguishes unknown cached state from a homeserver-confirmed absence.
A confirmed absence overrides a canonical alias from an older room summary.
Only confirmed absence of encryption permits the label "not encrypted"; redacted or malformed state never does.
Other absent state is labeled as confirmed absent without manufacturing configured values or permissions.
Restricted join-rule membership conditions use the same account's known canonical aliases, including rooms hidden by sidebar filters, and retain room IDs when no nonblank alias is known.
The formatter reads current room summaries so alias changes and removals apply without another metadata read; it performs no extra discovery requests.
`absence_check` reports refresh progress or failure independently of the previous confirmation.
An otherwise unknown server ACL does not add a row for an empty idle check; a confirmation, active check, or failure makes the row useful.

"Advertised aliases" remains the alternative aliases from room state beside the canonical alias.
"Aliases on your homeserver" and "Directory publication on your homeserver" are separate cached homeserver answers, never a federation-wide alias list.
Show each answer's last-answer time, expiration, refresh activity, failure, and future retry deadline when available.
An HTTP failure retains the old value with a stale-data notice; a conclusive denial or a newer answer with a field removed replaces it.
Malformed, oversized, forbidden, missing, unsupported, and future statuses remain explicit.
If either homeserver component is omitted, the other still renders and the missing component reads as unknown.
All server-supplied text uses the existing terminal sanitization and wrapping.
Visible source errors and server acquisition diagnostics remain the debugging surfaces; no private metadata bodies or credentials are logged.

### Delivery after joined-room details

The next PR replaces the sidebar's capped `space/children` read with bounded `space/hierarchy` pagination using `enrich=false`.
It preserves the shallow joined-room projection, account filtering, root ordering, and collapse state.
Incomplete walks must be visible rather than silently dropping children.

A later `/children [space]` popup will page direct children, explicitly request enrichment, display crawl progress, and preview only one unjoined child through `/discovery` at a time.
Browsing will not change the selected timeline or join a room.
An explicit join will reuse the existing flow with the originating account and routing hints.
Joined state will replace discovered state as authoritative, including field removals.
These follow-ups retain issue 624's separate discovery scope and do not belong to the first PR.

## Verification

The TUI unit suite exercises all five HTTP reads, scoped notifications, source coalescing, independent failures, bounded retries, reconnect, cancellation, and room/account changes.
Formatting tests cover known absence, unknown encryption, homeserver scope and status labels, stale values beside refresh failure, and denial replacing old data.
HTTP tests reject oversized bodies before decoding and keep old-server endpoint failures explicit.
Run formatting, TUI tests, strict clippy, and both TUI smoke profiles before publication.
The recorded demo still does not open `/whereami`; `docs/demo-coverage.md` records that gap.
