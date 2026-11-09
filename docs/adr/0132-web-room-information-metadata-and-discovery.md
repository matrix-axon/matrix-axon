# ADR 0132 — Web Room Information: typed metadata and progressive discovery

**Status:** Proposed for [issue 623](https://github.com/matrix-axon/matrix-axon/issues/623), the web step of tracker [issue 618](https://github.com/matrix-axon/matrix-axon/issues/618).
Implementation follows in two web PRs and one smoke PR (see Delivery).

## Context

The server side of issue 618 is complete:

- typed joined-room metadata, `GET …/rooms/{room_id}/metadata` ([issue 619](https://github.com/matrix-axon/matrix-axon/issues/619));
- authoritative member counts on `GET …/info` ([issue 620](https://github.com/matrix-axon/matrix-axon/issues/620));
- homeserver-local aliases and directory visibility, `GET …/homeserver_details`, and absence confirmation ([ADR 0126](0126-room-detail-acquisition-and-coverage.md), [issue 621](https://github.com/matrix-axon/matrix-axon/issues/621));
- the paged `GET …/space/hierarchy` with on-request crawls, and `GET …/rooms/{room_id}/discovery` for one unjoined room ([ADR 0128](0128-paged-space-hierarchy-and-unjoined-room-discovery.md), [issue 622](https://github.com/matrix-axon/matrix-axon/issues/622));
- the `room_details.changed` frame, which tells clients which of those reads to repeat.

The web client uses none of it.
Its Room Information panel (`clients/web/src/components/RoomInfoPanel.tsx`) reads `/info`, `/pinned`, `/space/children`, `/space/parents`, `/upgrade` and `/power_levels` when it opens, and again on certain `timeline.event` types and on every reconnect.
As a result:

- The "Full alias list" row is a fixed "Unavailable from current API" placeholder, and the name is one [ADR 0111](0111-cached-progressive-room-metadata.md) forbids.
- Encryption reads `/info`'s `encryption_algorithm` and shows **Unencrypted** when it is `null`, which only means Axon has no `m.room.encryption` tuple.
  ADR 0126 allows the spec default only after the homeserver confirms the absence.
- Join rule, history visibility and guest access fold "unknown" and "unset" into one "Unavailable".
- Member counts are fetched with `/info` and never shown.
- Unjoined space children come from the legacy `space/children` route, capped at 1,000 rows with no truncation signal, and render as their Matrix room ID because they have no synced name.
- The dialog for joining a linked room shows only the room ID.

The server ADRs fix the wire contract and the upstream bounds.
They leave open how the web client presents each state, which reads it makes and when, and how it keeps them fresh.
This ADR records those choices so the two implementation PRs share one specification.

## Decision

### 1. One store owns the panel's detail reads

A room-details store holds, for the room the panel shows, the latest `/metadata`, `/homeserver_details` and `/info` responses, and (decision 6) the space hierarchy pages and the selected child's `/discovery`.
It is keyed by account and room.
Changing the key aborts that key's in-flight reads with an `AbortController` and discards late responses with a key guard, as the panel's `forRoom` guard does today.

The panel opens immediately from the room-list summary (`RoomDto`) it already has, and each row fills in as its read returns.
No row waits for another read.

`/pinned`, `/space/parents`, `/upgrade` and `/power_levels` stay in the panel's existing loader; they are not part of issue 618.

### 2. Freshness: frames and reconnects, never polling

This is the panel's freshness story under AGENTS.md rule 8.

| Read | Re-read when |
|---|---|
| `/metadata` | a `room_details.changed` frame for this account and room names `metadata` |
| `/info` | the frame names `member_counts` |
| `/homeserver_details` | the frame names `homeserver_details` |
| `space/hierarchy` | the frame's `room_id` is the shown space and it names `space_hierarchy` |
| `/discovery` | the frame's `room_id` is the previewed room and it names `discovery`; while the response says `support: unsupported`, also a `space_hierarchy` frame for the space the room was opened from |
| all of the above | the socket reconnects, or the panel or preview is opened again |

The frame is a signal, never a diff, and the bus is lossy, so reconnect re-reads everything the store currently holds.
Unknown `sources` values are ignored.
The store tracks the last reconnect count it handled, instead of the panel's current `reconnects > 0` test, so a mount after an old reconnect does not issue a second round of reads.

A frame that arrives while a read for the same source is in flight marks that source dirty, and exactly one more read follows the current one.
The server already coalesces frames per account (100 ms quiet, 1 s at most), so the client adds no timer of its own.

The existing `timeline.event` triggers stay for the reads in the panel's loader.
`/metadata` does not also re-read on `m.room.*` timeline events: the server publishes `metadata` when the stored tuple changes, which is the event that matters.

### 3. Every server state has one label

A shared formatter maps each DTO state to a displayed value and an optional note, so rows never invent wording.
A displayed value is copyable only when it is real room data, never a state label.
This also replaces `DetailRow`'s magic-string test for placeholders ([issue 252](https://github.com/matrix-axon/matrix-axon/issues/252)): a row takes `value: string | null` and a separate label for the empty case.

**Metadata snapshots** (`CachedRoomMetadata_*`):

| Server state | Shown |
|---|---|
| `available` | the value |
| `partial` | the valid fields, noted "some fields withheld (malformed)" |
| `redacted: true` with content | the retained fields, noted "redacted; retained fields shown" |
| `redacted: true` without content | "Redacted" |
| `unknown`, `absence_confirmed_at` set | the Matrix default where the spec defines one ("Not encrypted", "No servers denied"); otherwise "Not set" |
| `unknown`, `absence_check.state` is `queued` or `running` | "Checking…" |
| `unknown`, `absence_check` idle with a `failure` or a future `next_attempt_at` | "Unknown (couldn't check)" |
| `unknown`, nothing else | "Unknown" |
| `unavailable` | "Unavailable (not retained)" |
| `invalid` | "Unavailable (malformed state)" |
| `too_large` | "Unavailable (state too large)" |

The wording matches the TUI's Room Information (`clients/tui/src/app/room_info.rs`) where both clients show the same state.

**Homeserver details** (`local_aliases`, `directory_visibility`):

| Server state | Shown |
|---|---|
| `available` or `partial` | the value; `partial` noted "some entries withheld" |
| any value with `expires_at` in the past | the value, noted "may be out of date" |
| `refresh.state` is `queued` or `running` | the current value or state, noted "Refreshing…" |
| `unknown` | "Unknown" (or "Checking…" while a refresh is queued or running) |
| `forbidden` | "Your homeserver didn't allow this lookup" |
| `not_found` | "Your homeserver doesn't know this room" |
| `unsupported` | "Not supported by your homeserver" |
| `too_large` | "Too many to show" |

A stale value stays visible next to a failed refresh; the failure is a note, not a replacement.

**Discovered summaries** (`DiscoveredRoomDto`):

| Server state | Shown |
|---|---|
| `status: available` | its fields, noted "From the space directory" with the `observed_at` time |
| `not_returned`, `forbidden`, `not_found` | "Not visible in the space directory" |
| `discovered: null` on an unjoined room | "Not looked up yet" |

`discovered.encryption: null` is "Unknown", never unencrypted.
Withheld fields (`withheld`) are left out, not shown as empty.

**Member counts:** "N joined · M invited"; `member_counts: null` is "Unknown (not yet observed)", never zero.
`observed_at` is not worded as a freshness time.

### 4. Aliases say whose list they are

- **Canonical alias** stays its own row, filled from the room-list summary until `/metadata` arrives.
- **Advertised aliases** are the canonical alias plus `alt_aliases` from `/metadata` `aliases`.
- **Aliases on your homeserver** are `local_aliases` from `/homeserver_details`.
- **Full alias list** is removed.

### 5. What the Details section shows

Encryption, Access, History visibility and Guest access come from `/metadata` instead of `/info`.
Access shows the join rule and, for `restricted` and `knock_restricted`, the rooms whose members may join.
A new Members row shows the member counts.

A collapsed **Advanced** group shows the room version, creators, and creation time (the `creation` snapshot's `origin_ts`, which for `m.room.create` is when the room was made), the predecessor room if any, directory visibility (`/homeserver_details`), and a server ACL summary (counts of allowed and denied servers, and whether IP literals are allowed).
The full ACL lists and power levels are not shown here; issue 659 and issue 324 cover roles.

`/homeserver_details` is read only for joined rooms, when the panel opens.
It schedules at most two upstream requests per room every 15 minutes (ADR 0126), so no extra gating is needed.

### 6. Space children come from the paged hierarchy

When the panel shows a space, its children come from `space/hierarchy` instead of `space/children`:

- The first page asks for 50 rows.
  "Load more" follows `next_cursor`, and the section reads "Showing N of T" from `total_children`.
- Rows are de-duplicated by room ID, because a link whose `order` changes mid-walk can appear twice.
- A row's name, avatar and topic follow ADR 0128's precedence: synced fields if `joined`; else `discovered` when its status is `available`; else synced fields; else the room ID.
  The two sources are never merged, and discovered values carry the provenance note from decision 3.
- On a `space_hierarchy` frame, the store re-reads the loaded prefix as one request with `limit` set to the number of loaded rows, at most 500, and keeps any rows beyond that.

A progress line comes from the page's `discovery` object, which is the source of truth after a reconnect:

| `discovery` | Shown |
|---|---|
| `state` `queued` or `running` | "Looking up rooms… (N found)" from `rooms_cached` |
| `outcome` `complete` | nothing beyond "Showing N of T" |
| `outcome` `truncated` | "The space directory listed only part of this space" |
| `outcome` `failed` | "Couldn't look up rooms; retrying at <time>" from `next_attempt_at` |
| `support` `unsupported` | "Your homeserver can't list this space's rooms" |

Parents keep the current `space/parents` read and the inference from the spaces store.

### 7. Enrichment is requested only by opening a space's information

The first hierarchy page is read with `enrich=true` when Room Information is open for a joined space and its children section is shown.
Opening the panel is the user asking, in ADR 0128's sense.
"Load more", frame re-reads and reconnect re-reads do not set `enrich`; the server's `discovery` object already says whether a crawl is running or due.
The server's TTL, backoff and unjoined-children rule bound the result, so reopening the panel cannot drive upstream traffic.

The room list and the space rail never set `enrich`.

### 8. One unjoined room is previewed on selection

Selecting an unjoined child opens the join dialog, seeded from that row's `discovered` summary.
The dialog then reads `/discovery` for that one room and shows name, topic, member count, join rule, encryption, room version and canonical alias, each per decision 3.
Closing the dialog aborts its read.

The Matrix link join prompt does the same for a room ID: one `/discovery` read, with the preview shown only when the room is `eligible` and its summary is `available`.
An alias target, or an ineligible room, keeps the current prompt; the server refuses to probe arbitrary rooms in any case.

No surface requests `/discovery` for more than one room at a time.

### 9. The spaces store stays on `space/children` for now

The spaces store (`clients/web/src/stores/spaces.ts`), which feeds the space rail and room-list grouping, keeps reading `space/children`.
Moving it to the paged route changes launch and reconnect cost for every joined space, which [issue 993](https://github.com/matrix-axon/matrix-axon/issues/993) is already examining, so both are designed together in a follow-up issue.
That follow-up, not this ADR, unblocks deprecating `space/children` (ADR 0128 decision 7).

## Alternatives considered

- **Poll the panel's reads while it is open.** The `room_details.changed` frame and reconnect re-reads cover every change the server can see; polling would add traffic without adding information (ADR 0126 decision 5).
- **Re-read every detail source on any frame for the room.** Simpler, but one hierarchy page during a crawl would re-read `/metadata` and `/homeserver_details` each time.
- **Show "Unencrypted" while `encryption` is `unknown`.** This is what the panel does today, and it is the inference both server ADRs forbid.
- **Merge discovered and synced fields into one value.** Fewer branches, but a user could not tell directory data from room state.
- **Read `/discovery` for every visible unjoined child.** Hierarchy pages already carry the same summaries; per-child reads are the fan-out ADR 0111 rules out.
- **Require a button before enriching.** Safer in principle, but the server already bounds crawls per space per TTL, and a panel that shows room IDs until a click is the problem this work exists to fix.
- **Move the spaces store in the same change.** It would let `space/children` be deprecated sooner, but it couples a launch-cost redesign (issue 993) to a panel change.

## Consequences

- Room Information shows scoped alias lists, member counts and honest encryption and access states, and fills them in progressively without polling.
- Unjoined children show names from the space directory, with provenance, and large spaces page instead of truncating silently.
- The web client gains a decoder for `room_details.changed` and a store that later surfaces (the spaces store, room settings) can reuse.
- `space/children` keeps two web callers' worth of traffic until the spaces store follow-up lands.
- The demo world needs an unjoined space child and a room with alternative aliases before these states can be recorded.

## Delivery

1. This ADR (docs only).
2. **Web PR A:** the frame decoder, the room-details store for `/metadata`, `/homeserver_details` and `/info`, decisions 3 to 5 in the panel, and the `DetailRow` change that closes issue 252.
3. **Smoke PR:** the demo corpus gains a space child the viewer has not joined and a room with alternative aliases.
4. **Web PR B:** decisions 6 to 8, demo scenes for the new states, and the spaces store follow-up issue.

Each web PR updates `docs/client-parity.md`, `docs/room-metadata.md`, `docs/demo-coverage.md` and `clients/web/README.md` as its behavior lands.

## Verification

- Unit tests for the formatter (every status in decision 3), the frame decoder, and the store (only the named sources are re-read; account and room filtering; reconnect; abort on key change; dirty-bit coalescing; `enrich` only on the first page of an open space panel).
- Panel and dialog tests for each state, including a room switch mid-read.
- Playwright layout tests at the supported desktop and mobile sizes, with mock-server routes for the new reads.
- A live run on the local demo stack: absence states fill in within seconds of opening a joined room, a space's crawl progress advances and unjoined children gain names, the preview fills from `/discovery`, and a reconnect restores state with no polling in the network log.
- With permission, a run against the production instance's largest space (thousands of children) to check paging, "Showing N of T", and one crawl per TTL.
