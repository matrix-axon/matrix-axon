# ADR 0135 — TUI room details and discovery

**Status:** Accepted; joined-room detail consumption and sidebar hierarchy pagination are implemented in the first two PRs for [issue 624](https://github.com/matrix-axon/matrix-axon/issues/624).
Discovery browsing remains for PR 3.

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
Freshness and retry-deadline notes compare the server/database's Unix-millisecond timestamps with the TUI host's wall clock.
They assume synchronized clocks; a client clock ahead of or behind the server can show or hide these advisory notes incorrectly.
Server acquisition still enforces its own backoff, and the TUI's HTTP retry schedule uses monotonic `Instant` deadlines, so this limitation affects presentation rather than request scheduling.
[Issue 1049](https://github.com/matrix-axon/matrix-axon/issues/1049) tracks a server-relative time basis for the web and TUI labels.
An HTTP failure retains the old value with a stale-data notice; a conclusive denial or a newer answer with a field removed replaces it.
Malformed, oversized, forbidden, missing, unsupported, and future statuses remain explicit.
If either homeserver component is omitted, the other still renders and the missing component reads as unknown.
All server-supplied text uses the existing terminal sanitization and wrapping.
Visible source errors and server acquisition diagnostics remain the debugging surfaces; no private metadata bodies or credentials are logged.

### Sidebar hierarchy pagination (PR 2)

The sidebar replaces its capped `space/children` read with bounded `space/hierarchy` pagination using `enrich=false`.
`app/spaces.rs` remains the membership and freshness owner, with four workers acquired before spawning and held for the whole walk.
This bounds concurrent walks but has a known latency cost: four slow walks can occupy every worker for up to 60 seconds, delaying other visible roots and pending navigation.
The startup selection deadline remains 30 seconds and can fall back to an available room while those walks continue.
The shallow joined-room projection, account filtering, root ordering, collapse state, and navigation are preserved.
Discovered summaries and the hierarchy's `joined` flag do not create navigation targets; the same account's room list remains authoritative.

Each walk requests 100 children per page and allows at most 200 pages, 2 MiB per response, 16 MiB in total, and 60 seconds overall.
Each page also retains the standard 30-second HTTP timeout.
The byte budget includes ignored discovery fields and error bodies, and no allocation is sized from `total_children`.
An exhausted byte budget fails before another HTTP request is sent.
Room IDs are deduplicated across pages; page and parent identities, child shape, cursor size, empty/control/whitespace cursors, and repeated cursors are checked.
A last page reporting more links than the walk collected is incomplete, including a concurrent addition behind the cursor; reconciliation starts a fresh walk.

Only a complete walk replaces cached membership, including confirmed empty membership.
Any failure or limit keeps the last complete grouping and marks the root `(incomplete; retrying)`; on first failure, rooms remain in provisional Ungrouped.
Retries start from the first page after 30 seconds.
A space that permanently exceeds a budget can repeatedly transfer up to 16 MiB per attempt under that fixed cooldown.
[Issue 1090](https://github.com/matrix-axon/matrix-axon/issues/1090) tracks bounded retry backoff separately from discovery browsing.
Explicit refresh and relationship changes bypass that delay.
Reconnect waits one second for connection stability and preserves failed-read deadlines while invalidating membership and re-reading root order.
Invalidation and root removal abort the walk immediately, and request IDs reject queued old outcomes even after a rejoin of the same account and room.
Account-filter changes can retain a result only under its originating account's cache key and cannot fulfill another account's navigation.
Older servers without the paged route show the same incomplete state rather than falling back to the silently capped endpoint.
The root marker is always visible; debug display additionally reports the account/space key and failure reason without logging page bodies or cursor values.

### Remaining discovery browsing (PR 3)

A later `/children [space]` popup will page direct children, explicitly request enrichment, display crawl progress, and preview only one unjoined child through `/discovery` at a time.
Browsing will not change the selected timeline or join a room.
An explicit join will reuse the existing flow with the originating account and routing hints.
Joined state will replace discovered state as authoritative, including field removals.
These follow-ups retain issue 624's separate discovery scope and do not belong to the sidebar PR.

## Verification

The TUI unit suite exercises all five HTTP reads, scoped notifications, source coalescing, independent failures, bounded retries, reconnect, cancellation, and room/account changes.
Formatting tests cover known absence, unknown encryption, homeserver scope and status labels, stale values beside refresh failure, and denial replacing old data.
HTTP tests reject oversized bodies before decoding and keep old-server endpoint failures explicit.
Sidebar HTTP tests cover more than 1,000 children, duplicate links, failed later pages, retry from the beginning, malformed/repeated cursors, byte/page/time budgets, cancellation, invalidation and rejoin races, and account isolation.
Run formatting, TUI tests, strict clippy, and both TUI smoke profiles before publication.
The recorded demo still does not open `/whereami`; `docs/demo-coverage.md` records that gap.
