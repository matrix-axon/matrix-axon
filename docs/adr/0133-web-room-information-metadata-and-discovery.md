# ADR 0133 — Web Room Information: typed metadata and progressive discovery

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
| `/discovery` | the frame's `room_id` is the previewed room and it names `discovery`; while the response says `support: unsupported` and the preview is still waiting for an answer, also any frame for this account naming `space_hierarchy` |
| all of the above | the socket reconnects, or the panel or preview is opened again |

The frame is a signal, never a diff, and the bus is lossy, so reconnect re-reads everything the store currently holds.
Unknown `sources` values are ignored.
The store tracks the last reconnect count it handled, instead of the panel's current `reconnects > 0` test, so a mount after an old reconnect does not issue a second round of reads.

A frame that arrives while a read for the same source is in flight marks that source dirty, and exactly one more read follows the current one.
The server already coalesces frames per account (100 ms quiet, 1 s at most), so the client adds no timer of its own.

When the homeserver has no room-summary endpoint, the `/discovery` read schedules a crawl of one joined space that links the room instead (ADR 0128 decision 5).
The server picks that space (the first linking space, by room ID, whose crawl is due), and the response does not say which one, so its summary arrives as a `space_hierarchy` frame for a space the client cannot predict: often not the space the row was opened from, and never a known space for a Matrix-link preview, which has none.
So while `support` is `unsupported`, a `space_hierarchy` frame for any space re-reads the one previewed room's `/discovery`, but only while the preview is still waiting for an answer: `discovered` is `null`, or its `expires_at` has passed.
Every `discovered` status is a conclusive answer with its own expiry (ADR 0128): `available`, and the denials `not_returned`, `forbidden` and `not_found`, which decision 3 shows as "Not visible in the space directory".
So once a read returns an unexpired `discovered` of any status, frames no longer re-read it; a reconnect or reopen still does.

Each of those reads can itself schedule a crawl.
The server's fallback runs on every read while `support` is `unsupported`, and picks a linking space whose crawl is due, so once one space's crawl ends the next read picks another ([issue 1035](https://github.com/matrix-axon/matrix-axon/issues/1035) narrows this to rooms whose summary is unknown or expired).
The bound is therefore: at most one crawl per joined space that links the previewed room, per `CRAWL_TTL` (six hours) or failure backoff, and none started by a frame once the summary has arrived.
A preview opened while another panel's crawl publishes frames is bounded the same way, since a re-read can only crawl a space that links the previewed room.
Decision 8 allows only one preview at a time, and the dirty bit below folds a burst of crawl-page frames into at most one read in flight and one after it.

The existing `timeline.event` triggers stay for the reads in the panel's loader.
`/metadata` does not also re-read on `m.room.*` timeline events: the server publishes `metadata` when the stored tuple changes, which is the event that matters.

### 3. Every server state has one label

A shared formatter maps each DTO state to a displayed value and an optional note, so rows never invent wording.
A displayed value is copyable only when it is real room data, never a state label.
This also replaces `DetailRow`'s magic-string test for placeholders ([issue 252](https://github.com/matrix-axon/matrix-axon/issues/252)): a row takes `value: string | null` and a separate label for the empty case.

**Metadata snapshots** (`CachedRoomMetadata_*`).
The rows are tested in order and the first match wins, so a snapshot that matches several (a redaction without content whose `status` is `unavailable`, or a confirmed absence whose 24-hour re-check is `queued`) has one label:

| Order | Server state | Shown |
|---|---|---|
| 1 | `redacted: true`, `content` present | the retained fields, noted "redacted; retained fields shown" |
| 2 | `redacted: true`, no `content` | "Redacted" |
| 3 | `available` | the value |
| 4 | `partial` | the valid fields; a withheld field reads "withheld (malformed)", and a list with withheld entries is noted "some entries withheld" |
| 5 | `unavailable` | "Unavailable (content not retained)" |
| 6 | `invalid` | "Unavailable (malformed state)" |
| 7 | `too_large` | "Unavailable (state too large)" |
| 8 | `unknown`, `absence_confirmed_at` set | the confirmed-absence value below, whatever `absence_check` says |
| 9 | `unknown`, `absence_check.state` is `queued` or `running` | "Checking…" |
| 10 | `unknown`, `absence_check` idle with a `failure` or a future `next_attempt_at` | "Unknown (couldn't check)" |
| 11 | `unknown`, otherwise | "Unknown (not cached)" |

A confirmed absence (row 8) shows what the specification says a room without that state event is like, for the types the panel shows:

| Snapshot | Shown when confirmed absent | Why |
|---|---|---|
| `encryption` | "Not encrypted" | No `m.room.encryption` means the room is not encrypted (ADR 0126). |
| `server_acl` | "No server ACL (no servers denied)" | No `m.room.server_acl` means no servers are denied (ADR 0126). |
| `join_rules` | "Invite only (no join rules set)" | The authorization rules admit a join only under a `public`, `invite`, `knock` or `restricted` rule, so without one only invited users can join. |
| `history_visibility` | "Shared (no history visibility set)" | The specification treats a room with no history visibility as `shared`. |
| `guest_access` | "Forbidden (no guest access set)" | The specification allows guests only when `m.room.guest_access` says `can_join`. |
| `aliases` | "None" | No `m.room.canonical_alias` advertises no aliases. |
| `creation` | "Unknown (not cached)" | Every room has a create event, so its confirmed absence names no default; the row falls back to row 11. |

Each default names the setting it stands in for, so a confirmed absence never looks like an explicit choice.
`power_levels` is not shown in the panel (decision 5), so it needs no default here.

Rows 1–7 and 11 use the TUI's phrases for the same states (`clients/tui/src/app/room_info.rs`: `redacted`, `(redacted; retained fields shown)`, `withheld (malformed)`, `(some entries withheld)`, `unavailable (content not retained)`, `unavailable (malformed state)`, `unavailable (state too large)`, `unknown (not cached)`), capitalized for the web panel.
The TUI does not read `absence_confirmed_at` or `absence_check`, so rows 8–10 and the defaults are new; issue 624 should adopt the same wording.

**Homeserver details** (`local_aliases`, `directory_visibility`).
A detail's label is one value, chosen by the first matching row of the first table, plus the notes from the second table that apply to it:

| Order | `status` and `refresh` | Value |
|---|---|---|
| 1 | `available` | the aliases ("None" for an empty list), or the visibility ("Public" or "Private") |
| 2 | `partial` | the valid aliases (note a) |
| 3 | `too_large` | "Too many to show" |
| 4 | `forbidden` | "Your homeserver didn't allow this lookup" |
| 5 | `not_found` | "Your homeserver doesn't know this room" |
| 6 | `unsupported` | "Not supported by your homeserver" |
| 7 | `unknown`, `refresh.state` `queued` or `running` | "Checking…" |
| 8 | `unknown`, `refresh` idle with a `failure` or a future `next_attempt_at` | "Unknown (couldn't check)" |
| 9 | `unknown`, otherwise | "Unknown (not cached)" |

Notes apply only to values from rows 1–6, which are answers the homeserver gave; rows 7–9 already describe the refresh.
They follow the value, in this order:

| Order | Condition | Note |
|---|---|---|
| a | `partial` (row 2) | "some entries withheld" |
| b | `refresh.state` `queued` or `running` | "Refreshing…" |
| c | `expires_at` in the past, and note b does not apply | "may be out of date" |
| d | `refresh` idle with a `failure` | "couldn't refresh" |

So a stale value stays visible next to a failed refresh ("Public · may be out of date · couldn't refresh"), and a refresh in progress replaces the staleness note rather than adding to it.

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

The advertised set is the canonical alias plus `alt_aliases` from `/metadata` `aliases` (ADR 0111), shown as two rows so the canonical alias appears once:

- **Canonical alias** stays its own row: `aliases.content.alias` labeled per decision 3, filled from the room-list summary until `/metadata` arrives.
- **Advertised aliases** lists `alt_aliases` only, as the TUI's `alias_lines` does: "None" when the list is empty or absent, never hidden, so it cannot be confused with a row that has not loaded.
  When the snapshot has no content, the row takes decision 3's label for the snapshot, like the canonical row.
- **Aliases on your homeserver** are `local_aliases` from `/homeserver_details`.
- **Full alias list** is removed.

### 5. What the Details section shows

Encryption, Access, History visibility and Guest access come from `/metadata` instead of `/info`.
Access shows the join rule and, for `restricted` and `knock_restricted`, the rooms whose members may join.
A new Members row shows the member counts.

A collapsed **Advanced** group shows:

- the room version;
- the creator: `content.creator` when present, otherwise the create event's `sender` (room versions from 11 drop the `creator` field and the sender is the creator, as `docs/room-metadata.md` and the TUI already read it), followed by any `additional_creators`;
- the creation time: the `creation` snapshot's `origin_ts`, which for `m.room.create` is when the room was made;
- the predecessor room, if any;
- directory visibility, from `/homeserver_details`;
- a server ACL summary: counts of allowed and denied servers, and IP literals as "allowed", "denied", or "not specified" from `allow_ip_literals`.
  An omitted field reads "not specified", as in the TUI; the API preserves omission (`docs/room-metadata.md`), so the panel does not substitute the spec's default for it.

The full ACL lists and power levels are not shown here; issue 659 and issue 324 cover roles.

`/homeserver_details` is read only for joined rooms: first when the panel opens, then again under decision 2 (on a `homeserver_details` frame, a reconnect, or a reopen), so a newly added local alias reaches "Aliases on your homeserver" without a reopen.
Each read schedules a refresh only when a detail is unknown or expired and no backoff is in force: an answer stays fresh for 15 minutes, a denial for an hour, and a failure backs off from one minute, doubling to 30 (ADR 0126).
So the panel can repeat the read on every open and frame without a gate of its own.

### 6. Space children come from the paged hierarchy

When the panel shows a space, its children come from `space/hierarchy` instead of `space/children`:

- The first page asks for 50 rows.
  "Load more" follows `next_cursor`, and the section reads "Showing N of T" from `total_children`.
- Rows are de-duplicated by room ID, because a link whose `order` changes mid-walk can appear twice.
- A row's name, avatar and topic follow ADR 0128's precedence: synced fields if `joined`; else `discovered` when its status is `available`; else synced fields; else the room ID.
  The two sources are never merged, and discovered values carry the provenance note from decision 3.
- On a `space_hierarchy` frame, or a reconnect, the store re-reads from the start as one request with `limit` set to the number of loaded rows, at least 50 and at most 500 (`SPACE_HIERARCHY_MAX_PAGE`).
  With 500 rows or fewer loaded, that one response covers the whole list: it replaces the loaded list, and its `next_cursor` becomes the list's cursor.
- With more than 500 rows loaded, a **frame** re-read does not shorten the list.
  It replaces the first 500 rows with the response, keeps the loaded tail, and keeps the tail's cursor; the response's own `next_cursor` is discarded.
  A tail row that the refreshed rows now contain is dropped, so each room appears once.
  The cursor is a keyset position (the last row's `order`, `origin_ts` and room ID; `SpaceHierarchyCursor`), so the tail's cursor still continues after the tail's last child and "Load more" neither repeats nor skips past it.
  The cost is that tail rows keep the fields they were loaded with, a child removed from the space can linger in the tail, and a child newly linked into the tail's range (or just ahead of it) is missing, until the next reconnect or reopen.
- A frame re-read keeps the tail only if the refreshed rows include every room ID of the first 500 loaded rows; otherwise it drops the tail as a reconnect does.
  A child linked (or re-ordered) into the first 500 positions pushes the old rows near position 500 out of the response, and the kept tail starts after the old 500th row, so those rows would be in neither part and vanish until a reconnect or reopen.
  A child unlinked from the first 500 drops the tail too, since the client cannot tell a removal from a shift.
  Rows carry `order` and room ID but not `origin_ts`, so the client cannot compare keyset positions directly; the room-ID check needs only what it has.
  Crawl frames change `discovered` fields, not links, so they never fail this check; only a real link change near the head collapses the list.
  A crawl publishes `space_hierarchy` for each changed page, about once a second, so collapsing to 500 rows on every frame would undo the user's paging while they scroll.
- A **reconnect or reopen** re-read of a list past 500 rows replaces it with the first 500 and that response's cursor, dropping the tail.
  A tail kept across a reconnect may be missing any number of changes, while a frame re-read follows a change the client has just been told about.
- "Showing N of T" counts the rows actually loaded.

A progress line comes from the page's `discovery` object, which is the source of truth after a reconnect.
Every crawl that ends without success has `outcome: failed`, so the rows are tested in order and the first match wins:

| Order | `discovery` | Shown |
|---|---|---|
| 1 | `state` `queued` or `running` | "Looking up rooms… (N found)" from `rooms_cached` |
| 2 | `support` `unsupported` | "Your homeserver can't list this space's rooms" |
| 3 | `outcome` `failed`, `failure` `forbidden` | "Your homeserver didn't allow listing this space's rooms" |
| 4 | `outcome` `failed`, `failure` `not_found` | "Your homeserver couldn't find this space's room list" |
| 5 | `outcome` `failed`, any other `failure` | "Couldn't look up rooms; trying again at <time>" from `next_attempt_at` |
| 6 | `outcome` `in_progress` (with `state` `idle`) | "Room lookup paused; resuming…" (decision 7 resumes it) |
| 7 | `outcome` `truncated` | "The space directory listed only part of this space" |
| 8 | `outcome` `complete`, or `null` | nothing beyond "Showing N of T" |

Rows 2–4 are conclusive: their `next_attempt_at` is the end of a 24-hour (unsupported) or one-hour (denial) hold, not a retry time, so it is not shown as one.
Only row 5 is a transient failure, and decision 7 makes its "trying again" true.

Parents keep the current `space/parents` read and the inference from the spaces store.

### 7. Enrichment is requested only by opening a space's information

The first hierarchy page is read with `enrich=true` when Room Information is open for a joined space and its children section is shown.
Opening the panel is the user asking, in ADR 0128's sense.
A reopen therefore schedules a crawl only when the previous one is due (its six-hour `CRAWL_TTL` has passed, or its failure backoff has) and the space still links an unjoined child.
That is the bound ADR 0128 sets ("cannot drive upstream traffic past `CRAWL_TTL` and the failure backoff"), and it is the intended "user asking": a later open is not free, but it costs at most one crawl per space per TTL.

"Load more", frame re-reads and reconnect re-reads do not set `enrich`, with two exceptions that keep an open panel's crawl from stalling:

- **A crawl the server paused.** A crawl page that the worker's queue drops does not keep running: the crawl stays `in_progress`, becomes due, and the worker publishes `space_hierarchy`, but only the next `enrich` read resumes it.
  The same is true after a server restart, since `state` is held in memory and reads `idle`.
  So when a re-read shows `outcome: in_progress` with `state: idle`, the store sends one `enrich=true` read.

  A healthy multi-page crawl does not look like this to a frame-prompted re-read.
  The worker queues a crawl's next page before it publishes the page's frame (`step` in `crates/axon-sync/src/acquisition.rs`), so the re-read that frame prompts sees `queued`.
  A page waiting for its homeserver gate permit also reads `queued`.
  The only between-pages `idle` is the instant between the worker ending one page and queuing the next, with no `await` between them.
  A read that lands there was prompted by something else, and its `enrich` read costs no extra request: the queue holds one job per room and kind, so the scheduled page and the continuation are the same job.
  The client accepts that residual rather than trying to tell the two apart.
- **A transient failure.** When a re-read shows progress row 5, the store sets one timer while the panel stays open, and sends one `enrich=true` read when it fires.
  `next_attempt_at` is a server time and responses carry no server clock, so the delay is `next_attempt_at` minus the client's clock, clamped to between 5 seconds and 30 minutes (the longest backoff).
  If the client clock is fast, the read finds the crawl still waiting on the same `next_attempt_at`, and the timer is armed once more at twice its previous delay (at least 10 s, then 20 s, and so on, up to 30 minutes).
  A fast clock therefore costs a few local reads, never a loop; a slow clock delays the retry by the skew.
  The progress label shows `next_attempt_at` as given, so skew moves the displayed time by the same amount.

The resume sends at most one `enrich` read per frame or reconnect that prompted it, and its response never prompts another.
The timer sends one per firing, and only the re-arming above follows from its response.
The server applies its own due check to every such read, so one it declines costs a local read and nothing upstream.
Closing the panel or switching rooms cancels the timer.

The room list and the space rail never set `enrich`.

### 8. One unjoined room is previewed on selection

Selecting an unjoined child opens the join dialog, seeded from that row's `discovered` summary.
The dialog then reads `/discovery` for that one room and shows name, topic, member count, join rule, encryption, room version and canonical alias, each per decision 3.
Closing the dialog aborts its read.

The Matrix link join prompt does the same for a room ID: one `/discovery` read, with the preview shown once the room is `eligible` and its summary is `available`.
While the prompt is open it is re-read under decision 2, like the dialog; it has no originating space, which is one reason the unsupported fallback re-reads on a `space_hierarchy` frame for any space.
An alias target, or an ineligible room, keeps the current prompt; the server refuses to probe arbitrary rooms in any case.

No surface requests `/discovery` for more than one room at a time.

### 9. The spaces store stays on `space/children` for now

The spaces store (`clients/web/src/stores/spaces.ts`), which feeds the space rail and room-list grouping, keeps reading `space/children`.
Moving it to the paged route changes launch and reconnect cost for every joined space, which [issue 993](https://github.com/matrix-axon/matrix-axon/issues/993) is already examining, so both are designed together in a follow-up issue.
That follow-up, not this ADR, unblocks deprecating `space/children` (ADR 0128 decision 7).

## Alternatives considered

- **Poll the panel's reads while it is open.** The `room_details.changed` frame and reconnect re-reads cover every change the server can see; polling would add traffic without adding information (ADR 0126 decision 5).
- **Re-read the preview's `/discovery` on every `space_hierarchy` frame while `support` is `unsupported`.** Each read can schedule the server's fallback crawl of another linking space, so frames from one crawl would chain into crawls of every linking space even after the summary had arrived.
- **Re-read the preview's `/discovery` only on frames for the space the row was opened from.** The server, not the client, picks the space it crawls for an unsupported summary, and does not report it; a Matrix-link preview has no such space at all.
- **Keep the loaded tail on every re-read, including a reconnect.** A tail kept across a reconnect may be missing any number of changes, and nothing re-reads it until the panel is reopened.
- **Drop rows past 500 on every frame re-read.** Consistent, but a crawl publishes a frame about once a second, so a user who had paged past 500 rows would see the list collapse while scrolling.
- **Detect the gap by keyset comparison** (drop the tail when the refreshed 500th row sorts before the tail's first). Exact, but rows carry no `origin_ts`, the tie-breaker after `order`, so the client cannot order rows; it would need to decode the opaque cursor or a new field.
- **Continue a kept tail from the refreshed page's `next_cursor`.** That cursor continues after the new 500th child, not after the tail, so "Load more" would repeat or skip children.
- **Never set `enrich` after the first read.** A crawl page dropped from the server's queue, or a transient failure, would then stall until the panel is reopened, while the panel showed it as finished.
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

- Unit tests for the formatter (every row of decision 3's tables, including the overlapping cases its order settles, and every progress row in decision 6), the frame decoder, and the store: only the named sources are re-read; account and room filtering; reconnect; abort on key change; dirty-bit coalescing; `enrich` only on the first page of an open space panel, plus one resume read for a paused crawl, none for a frame whose re-read sees the next page `queued`, and timer reads for a transient failure that start at no less than 5 s and double when the server declines (a fast client clock), never a resume read prompted by its own response; a frame re-read past 500 loaded rows keeps the tail and its cursor when the refreshed rows contain every room ID of the first 500 loaded rows, and drops it when a child linked into the first 500 positions pushes a loaded row out (no row vanishes), while a reconnect re-read always drops it; and, while `support` is `unsupported`, a `space_hierarchy` frame for any space re-reads the previewed room's `/discovery` until an unexpired `discovered` of any status arrives (`available`, or a `not_returned`, `forbidden` or `not_found` denial), and not after.
- Panel and dialog tests for each state, including a room switch mid-read.
- Playwright layout tests at the supported desktop and mobile sizes, with mock-server routes for the new reads.
- A live run on the local demo stack: absence states fill in within seconds of opening a joined room, a space's crawl progress advances and unjoined children gain names, the preview fills from `/discovery`, and a reconnect restores state with no polling in the network log.
- With permission, a run against the production instance's largest space (thousands of children) to check paging, "Showing N of T", and one crawl per TTL.
