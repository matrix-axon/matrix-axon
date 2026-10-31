# ADR 0128 — Paged space hierarchy and unjoined-room discovery

**Status:** Proposed; tracked in [issue 622](https://github.com/matrix-axon/matrix-axon/issues/622), part of [issue 618](https://github.com/matrix-axon/matrix-axon/issues/618).
Records the discovery policy that [ADR 0111](0111-cached-progressive-room-metadata.md) deferred, and settles the question [ADR 0126](0126-room-detail-acquisition-and-coverage.md) decision 3 left to issue 622: which unjoined rooms acquisition may reach.

## Context

Room Information shows a space's children from `GET /v1/accounts/{account_id}/rooms/{room_id}/space/children` (ADR 0084).
That read joins each `m.space.child` link with the child's own cached `room_state`.
An unjoined child usually has no cached state, so clients fall back to its Matrix room ID (issue 618).

**Silent truncation.**
The read is single-shot and capped at `SPACE_HIERARCHY_CAP` (1,000) rows after the MSC1772 sort.
Its response is a bare array in `data`, so a caller cannot tell a 1,000-child space from a truncated one.
The TUI refuses responses above 1,000 children or 1 MiB and keeps retrying the space every 30 seconds without making progress (issue 553).

**What the homeserver can tell us.**
Two client-server endpoints describe rooms the account has not joined:

- [`GET /_matrix/client/v1/rooms/{roomId}/hierarchy`](https://spec.matrix.org/v1.19/client-server-api/#get_matrixclientv1roomsroomidhierarchy) pages through a space's children.
  Each entry carries the same summary fields: name, topic, avatar, canonical alias, joined-member count, join rule, room type, world-readable, guest access, and, since v1.15, the optional `encryption` and `room_version`.
- [`GET /_matrix/client/v1/room_summary/{roomIdOrAlias}`](https://spec.matrix.org/v1.19/client-server-api/#get_matrixclientv1room_summaryroomidoralias) returns the same fields for one room.
  It is stable since v1.15; older Synapse versions serve it as `unstable/im.nheko.summary` (MSC3266).

Synapse's implementation shapes the policy below:

- Hierarchy pages hold at most 50 rooms, whatever `limit` asks for.
- The first page includes the requested space itself.
- A `from` token belongs to a pagination session that lasts 5 minutes.
  An unknown or expired token, or a change to `max_depth` or `suggested_only` between pages, returns `400 M_INVALID_PARAM`.
  A change to `limit` is allowed.
- Remote children are summarised over federation, trying up to three servers per room, so one hierarchy page can cost several federation requests.
  This is the same kind of upstream cost that starved the production Synapse in October (ADR 0121, ADR 0122).
- Hierarchy requests are rate limited per requester (5 per second, burst 10).
- Every entry carries its own `children_state`, so a page that includes a large subspace can be far larger than 50 small summaries.

**Machinery to reuse.**
ADR 0126 decision 4 specifies one paced acquisition worker per account behind the `HomeserverGate`.
Its delivery step 2 (issue 621) has not landed; this ADR depends on it.
ADR 0126 decision 5's `room_details.changed` frame (shipped in PR 882) is the notification channel.

**Nested spaces** are flat in every client today (issue 808).
A joined subspace's children are already in local `room_state`, so nesting needs no discovery; it waits on the product decisions in issue 808.
An unjoined subspace's children could only come from a deeper hierarchy request, which multiplies the federation cost above.

## Decision

### 1. A paged hierarchy read replaces the capped children read

Add `GET /v1/accounts/{account_id}/rooms/{room_id}/space/hierarchy`.
It reads only local data and never waits for the homeserver.

- **Query:** `limit` (default 100, maximum 500), `cursor` (opaque, from a previous page's `next_cursor`), and `enrich` (boolean, default `false`; decision 2).
- **Order and paging:** MSC1772 order as today (`order` ascending with absent last, then `origin_ts`, then child room ID), paged by a keyset cursor over those three values.
  The cursor records whether the last row's `order` was absent, so the absent-last group pages correctly.
  Each page starts strictly after the last row the previous page returned, so a link added or removed between pages neither duplicates nor skips any other link.
  A link whose `order` or `origin_ts` changes between pages (a replaced `m.space.child` event) moves in the sort and can appear twice or not at all in one walk; clients de-duplicate by room ID, and the next walk is consistent.
- **Response:** `{ room_id, children, next_cursor, total_children, discovery }`.
  - `total_children` counts the space's local child links (non-empty `m.space.child` content), so a client can show "120 of 1,340".
  - `next_cursor` is `null` only on the last page; there is no total cap, so nothing is dropped silently.
  - `discovery` is the space's crawl status (decision 3), returned whether or not `enrich` is set, so it is the source of truth after a reconnect.
- **Each child** carries every `SpaceChildDto` field with its current meaning (cached synced state, `null` when unknown), plus:
  - `parent_room_id`: the space whose link produced the row; always the requested space for now (decision 8).
  - `joined`: whether the account is joined to the child.
  - `discovered`: the cached discovery summary (decision 4), or `null` when Axon has no discovery row for the child.
    A row whose status is not `available` (`not_returned`, `forbidden`, or `not_found`) is returned as a non-null `discovered` with that `status`, its provenance, and no summary fields, so a client can tell "never discovered" from "discovered and since withdrawn".
    It is always `null` for a joined child, because synced state is authoritative for joined rooms.

Clients display a child's synced fields when it is joined, and `discovered` fields when it is not and `discovered.status` is `available`, falling back to the synced fields and then to the room ID.
The server does not merge the two, so a client can always tell discovered data from synced state (ADR 0111).

The existing `space/children` route stays as it is, including its cap and bare-array response (decision 7).

### 2. Discovery runs only when a client asks, and only for joined spaces

A hierarchy read with `enrich=true` schedules a crawl of the requested space when all of these hold:

- the account is joined to the space;
- the space has no crawl; or its last crawl ended (`complete`, `truncated`, or a conclusive denial) and its `expires_at` has passed; or its crawl is `in_progress` or `failed`, is not already queued or running, and can resume (decision 3);
- the crawl's `next_attempt_at`, if any, has passed.

Otherwise the read is pure, so client polling cannot drive upstream traffic past `CRAWL_TTL` and the failure backoff.
A `truncated` crawl is conclusive until `expires_at`, exactly like a `complete` one, so a space with more than `MAX_PAGES` pages of children is crawled once per `CRAWL_TTL`, not on every read.
Reads without `enrich`, such as room-list grouping, never cause discovery.
Spaces are never crawled in the background.

A room-summary read (decision 5) schedules a request only for a room that the account has not joined and that a space the account has joined links to with a current `m.space.child` event.
Neither endpoint can be used to probe arbitrary room IDs or aliases through the homeserver.
ADR 0126's homeserver details and absence confirmations stay limited to joined rooms.

### 3. Hierarchy crawls are paged jobs on the acquisition worker

A crawl is a sequence of `hierarchy(space)` jobs on the account's ADR 0126 worker, one job per page.
After committing a page, the job queues the next page behind whatever is already waiting, so a panel opened during a long crawl is served between pages.
The worker's `JOB_INTERVAL` (250 ms) keeps one account at four requests a second or fewer, under Synapse's per-requester hierarchy limit of five.

- **Crawl outcome:** each crawl has an outcome, persisted in `space_discovery`:
  - `in_progress`: pages remain and the cursor is saved;
  - `complete`: the last page had no `next_batch`;
  - `truncated`: the crawl stopped at a limit;
  - `failed`: a request failed and is waiting for `next_attempt_at`.

  `complete`, `truncated`, and a conclusive denial end the crawl until `expires_at` (decision 2).
  An `in_progress` crawl whose continuation the queue's drop-oldest rule evicted, or a `failed` crawl past its `next_attempt_at`, resumes from the saved cursor on the next `enrich` read, restarting if the cursor's session has expired.

- **Generations:** each crawl started from the first page, including a restart after an expired cursor, takes a new generation number and a new `started_at`.
  Resuming from a saved cursor keeps the generation.
- **Request:** `max_depth=1`, `suggested_only=false`, `limit` at the crawl's current page limit, and `from` when resuming.
  Every request takes a gate permit as background work, with ADR 0126's reqwest path, bounded body reader, and access-token handling.
- **Page size:** a crawl starts at `PAGE_LIMIT`.
  A body over `MAX_PAGE_BYTES` is refused before parsing, the crawl's page limit is halved and persisted, and the same cursor is retried as the next job; Synapse accepts a changed `limit` within a session.
  The lower limit applies to the rest of the crawl and is never raised within it, so a crawl makes at most six size retries in total (50 down to 1), not six per page.
  A single-room page still over the limit ends the crawl `truncated` with failure `too_large`.
- **Parsing:** each entry is parsed into a summary that skips `children_state` and any other unknown fields.
  The requested space's own entry is skipped.
  A field that fails validation is withheld from the summary, as decision 4 describes, and the rest of the entry is kept.
  Only an entry with no valid `room_id`, or one that is not a JSON object, is skipped whole and counted as `unidentified`.
- **Linking:** a returned room is cached only if the space has a current local `m.space.child` link to it.
  A room the homeserver returns that Axon has no link for is counted as `unlinked` and dropped: local state alone decides membership, ordering, and the suggested flag.
  A local link the homeserver did not return stays in the hierarchy read with its synced fields, or with no summary at all.
- **Returned set:** a `space_discovery_children` table records, for each (account, space, child), the generation in which the crawl last returned that child.
  Every linked entry with a valid `room_id` counts as returned, including one with withheld fields, so a malformed field can never make a good summary `not_returned`.
- **Continuation:** the crawl ends `complete` when a page has no `next_batch`.
  It ends `truncated` after `MAX_PAGES` pages, or when a `next_batch` repeats a token already used in this generation (failure `pagination_loop`).
  An `M_INVALID_PARAM` on a resumed page restarts the crawl as a new generation once (`MAX_CURSOR_RESTARTS`); a second one makes the crawl `failed` with `cursor_expired` and takes the backoff.
- **Outcomes:**
  - `403` or `404 M_NOT_FOUND`: the crawl ends with failure `forbidden` or `not_found` and the conclusive-denial expiration; cached summaries for the space's children are kept until a later complete crawl or the leave cleanup (decision 6) removes them.
  - `404` or `405 M_UNRECOGNIZED`: the account's homeserver lacks the endpoint; `support` becomes `unsupported` for `UNSUPPORTED_TTL`.
  - Any other failure leaves committed pages in place, keeps the cursor, makes the crawl `failed`, and takes ADR 0126's failure backoff.
- **Completion:** when a crawl ends `complete`, one transaction does two things:
  - **Omission:** every linked child whose returned generation is older than the current one, and whose `room_discovery` row is `available` with an `observed_at` before this generation's `started_at`, becomes `not_returned` and its fields are cleared.
    The homeserver omits rooms the account can no longer see, so this is how a loss of access reaches the cache.
    The `observed_at` condition keeps a summary that another space's crawl or a room-summary job refreshed during this crawl.
    If the generation skipped any `unidentified` entry, omission is not applied, because the skipped entry may have been one of the unreturned children.
  - **Orphans:** every `room_discovery` row that no joined space links to any more is deleted, along with `space_discovery_children` rows for links that no longer exist.

  A `truncated` or `failed` crawl does neither; it cannot prove absence (issue 553).

- **Fencing:** each page commits in one transaction that first takes a share lock on the space's `room_summaries` row and checks that the account exists and that the space's `membership_epoch` (ADR 0126 decision 4) is the one recorded when the job started.
  A leave of the space must update that row, so it waits for the commit and its cleanup then removes what the commit wrote.
  In the same transaction, a child the account has joined is skipped, so a join completed before the commit wins.
  A join that races the commit cannot leave a visible stale row: reads withhold discovery for joined rooms, and the child's later leave deletes its row (decision 6).
  No lock is held across the request.

The crawl status in `discovery` reports `state` (`idle`, `queued`, or `running`), the crawl outcome, `pages`, the current page limit, `rooms_cached`, `started_at`, `completed_at`, `expires_at`, `support`, the last failure's category, and `next_attempt_at`.
As in ADR 0126, `queued` and `running` live only in memory, so a restart cannot leave a crawl permanently pending, while outcomes, cursors, page limits, expiration, and backoff persist.

### 4. Discovery summaries are a separate account-scoped cache

A new `room_discovery` table holds one row per account and room, shared by every space that links the room.
It is never merged into `room_state` or `room_summaries`.

- **Fields:** everything a hierarchy entry or room summary returns together: name, topic, avatar URL, canonical alias, joined-member count, join rule, room type, world-readable, guest access, `encryption`, and `room_version`.
  A field missing from the latest answer is stored as missing, so a newer answer that drops a field removes it rather than leaving the older value.
- **Security-relevant absence:** a missing `encryption` field means unknown, never unencrypted, and the DTO says so (`encryption: null` with no default applied).
- **Provenance:** `source` (`hierarchy` or `summary`), `observed_at`, and `expires_at`, all from the server clock.
- **Status:** `available`, `not_returned`, `forbidden`, or `not_found`; every status other than `available` carries no fields.
  The row is shared by every space that links the room, so the latest conclusive answer wins: a crawl of another space that returns the room makes it `available` again.
- **Validation:** each string field has a byte limit; the canonical alias must parse as a room alias and the avatar as an `mxc://` URI; the member count must be a non-negative integer.
  A field that fails is withheld and named in `withheld`, and the rest of the summary is kept.
  This is the only validation outcome for a field; decision 3 skips an entry only when it has no valid room ID.
- **Refresh state** for room-summary requests (decision 5) uses ADR 0126's refresh object: `idle`, `queued`, or `running`, the last failure's category, and `next_attempt_at`.

A crawl's per-space state lives in a separate `space_discovery` table keyed by account and space, and the generation in which each child was last returned in `space_discovery_children` (decision 3).

### 5. Room summaries fill in one selected unjoined room

Add `GET /v1/accounts/{account_id}/rooms/{room_id}/discovery`.
It returns the room's `room_discovery` row and refresh state immediately, and it schedules a `summary(room)` job when the room qualifies under decision 2 and its summary is unknown, expired, or due.
A client calls it when a user selects an unjoined room, such as a space child in Room Information.

- **Capability:** the worker reads the account's `/versions` (already cached by the SDK).
  It uses the stable path when the server advertises v1.15 or later, the `im.nheko.summary` unstable path when only that feature is advertised, and otherwise records `unsupported` without making a request.
- **Request:** `via` is taken from the linking `m.space.child` events, at most `MAX_VIA_SERVERS` distinct servers.
  The response's `membership` field is ignored; local membership is authoritative.
- **Outcomes:** `200` replaces the row with `source: summary`; `403` and `404 M_NOT_FOUND` record `forbidden` or `not_found` with the conclusive-denial expiration and clear the fields; `404` or `405 M_UNRECOGNIZED` records `unsupported`; anything else is a failure with backoff.
- **Fallback:** when the summary endpoint is unsupported, the room's summary comes only from crawls of the spaces that link to it; the read schedules a crawl of one linking joined space as if that space had been read with `enrich=true`.
- **Fencing:** the result commits in one transaction that checks the account still exists, the room is still unjoined, and a joined space still links to it, taking a share lock on that space's `room_summaries` row as decision 3 does.

Summaries are never requested for several children at once; hierarchy pages serve that need.

### 6. Join, leave, and removal transitions

- **Joining a discovered room** deletes its `room_discovery` row in the same transition that marks it joined, and reads withhold discovery for joined rooms regardless; synced state is authoritative from that point, including fields it does not carry.
- **Leaving a joined room** deletes any `room_discovery` row for it in the `hidden_left` transition, so a row written by a commit that raced the join never outlives the membership; the room becomes eligible for discovery again if a joined space links to it.
- **Leaving a space** deletes its `space_discovery` and `space_discovery_children` rows and every `room_discovery` row no joined space still links to, in the `hidden_left` transition that ADR 0126 uses for its cleanup; it also increments the space's `membership_epoch`, which fences any page in flight.
- **Removing a child link** leaves the summary in place until the orphan step of the next complete crawl of any space (decision 3) or a leave cleanup deletes rows no joined space links to; reads never show it meanwhile, because they list only linked children.
- **Removing the account** cascades.

### 7. The legacy children route is unchanged and becomes deprecated

`space/children` keeps its response shape, its 1,000-row cap, and its local-only behavior.
Its response is a bare array, so a cursor or truncation flag would break existing decoders, and a header-only signal would still need client changes.
The OpenAPI description states the cap and points to `space/hierarchy`; the route is marked deprecated once the web client (issue 623), the TUI (issue 624), and the oversized-space work (issue 553) have moved to the new route.
It is not removed, under the additive `/v1/` policy (ADR 0099).

### 8. Nested spaces stay out of scope, with room to add them

This ADR crawls and reads direct children only (`max_depth=1`).
The paged read is shaped so that nesting can be added without another endpoint: rows carry `parent_room_id`, and a later `max_depth` parameter can walk joined subspaces from local state in depth-first MSC1772 order, with a cycle rule, once issue 808 decides how clients present subspaces.
Discovering the children of unjoined subspaces would need deeper hierarchy requests and is not planned.

### 9. Change notification

`room_details.changed` (ADR 0126 decision 5) gains two sources:

- `space_hierarchy`, with the space's room ID: a committed page changed at least one cached summary, or the crawl reached a conclusive state (`complete`, `truncated`, a failure, or `unsupported`).
  Re-read `space/hierarchy`.
- `discovery`, with the child's room ID: a room-summary job changed that room's row.
  Re-read `…/discovery`.

A page that only re-observes identical summaries publishes nothing beyond its conclusive-state frame.
Frames go through the existing per-account coalescer.

### 10. Limits

In keeping with ADR 0121 decision 3, the limits are constants:

| Constant              | Value | Why                                                                                       |
| --------------------- | ----- | ----------------------------------------------------------------------------------------- |
| `PAGE_LIMIT`          | 50    | Synapse's server-side maximum; asking for more changes nothing                            |
| `MAX_PAGES`           | 40    | 2,000 children per crawl; a larger space is reported `truncated`, never cut silently      |
| `MAX_PAGE_BYTES`      | 2 MiB | Fifty summaries plus moderate `children_state`; larger pages retry at half size           |
| `MAX_CURSOR_RESTARTS` | 1     | One restart covers a crawl paused past Synapse's 5-minute session                         |
| `HIERARCHY_TIMEOUT`   | 30 s  | One page can wait on federation for several remote children; ADR 0126's 10 s is too short |
| `SUMMARY_TIMEOUT`     | 10 s  | One room; matches ADR 0126's `REQUEST_TIMEOUT`                                            |
| `CRAWL_TTL`           | 6 h   | Space membership and child names change slowly; reopening a panel within it costs nothing |
| `SUMMARY_TTL`         | 1 h   | A selected room is being looked at; one refresh per hour per room bounds repeat reads     |
| `MAX_VIA_SERVERS`     | 3     | Matches the servers Synapse tries per room                                                |
| `MAX_NAME_BYTES`      | 1 KiB | Display names beyond this are pathological                                                |
| `MAX_TOPIC_BYTES`     | 8 KiB | Topics are longer than names but are shown as summaries                                   |

The conclusive-denial expiration, `UNSUPPORTED_TTL`, failure backoff, job interval, and queue capacity are ADR 0126's.

### 11. Diagnostics

The worker logs crawls and summary jobs under `axon_sync::acquisition` with `account_id`, the space or room ID, `kind`, `outcome`, `elapsed_ms`, page number, page limit, rooms cached, `unlinked`, `unidentified`, withheld fields, and restarts.
It logs at debug per page and at warn for repeated failures, `pagination_loop`, and `too_large`.
It never logs names, topics, aliases, response bodies, or tokens.

## Alternatives considered

- **Add a cursor and truncation flag to `space/children`.** The bare-array response cannot carry them without breaking every existing decoder; decision 7.
- **Signal truncation with a response header on `space/children`.** Additive, but clients would still need changes to act on it, and they move to the paged route anyway.
- **Crawl every joined space in the background.** Keeps every panel warm, but turns room-list loading into federation load on spaces nobody opened, which is the failure mode ADR 0121 and ADR 0122 exist to prevent.
- **Discover nested subspaces now (`max_depth` above 1).** Multiplies federation cost, and the client presentation is undecided (issue 808).
- **Fetch a room summary per child.** One request per child, where one hierarchy page covers fifty; issue 622 rules it out.
- **Run a whole crawl as one job.** Simpler, but a 40-page crawl would hold the account's worker for minutes and starve panel jobs behind it.
- **Merge discovered fields into the synced fields server-side.** Fewer client branches, but clients could no longer tell discovered data from synced state, which ADR 0111 requires.
- **Cache rooms the homeserver returns without a local link.** More names, but the hierarchy would then decide membership, and a stale or hostile response could add children the space's state does not list.
- **Keep the last known summary when a complete crawl omits a child.** Survives a federation hiccup, but keeps showing a room the homeserver no longer vouches for after access is lost.
- **Allow summaries for any room ID or alias.** Useful for previews before joining, but turns the endpoint into a probe of arbitrary rooms through the account's homeserver.
- **Store discovery results in `room_state`.** Rejected by ADR 0111: discovered summaries have different access, completeness, and freshness semantics and no event provenance.

## Consequences

- A space of any size lists every local child, in order and page by page, with an honest total; nothing is cut silently.
- Unjoined children gain names, avatars, topics, member counts, and join rules progressively after a client asks for enrichment, each crawl page arriving as a `room_details.changed` frame.
- Upstream cost is bounded by explicit requests, `CRAWL_TTL`, the gate, the per-account worker, and the page and crawl limits; room-list grouping costs nothing upstream.
- Loss of access reaches the cache through complete crawls and conclusive summary answers, and leaving a space removes its discovery data.
- New schema: `room_discovery`, `space_discovery`, `space_discovery_children`, and transition cleanup in the leave and join triggers; forward-only migrations with no backfill.
- Clients have one more read to adopt; the legacy route keeps working for older clients indefinitely.

## Delivery

Each step is one server-silo PR, with OpenAPI, the regenerated web schema, `docs/room-metadata.md`, and `docs/client-parity.md` updated in the same PR:

1. **Paged hierarchy read:** decisions 1 and 7, local only; `enrich` is accepted and ignored, and `discovery` is `null`.
   Needs nothing else, and unblocks issue 553.
2. **ADR 0126 step 2 (issue 621):** the acquisition worker, homeserver details, absence confirmation, and `membership_epoch`.
3. **Hierarchy discovery:** decisions 2, 3, 4, 6, 9 (`space_hierarchy`), 10, and 11.
4. **Room summaries:** decision 5 and the `discovery` source; closes issue 622.

Client adoption follows as separate PRs: web (issue 623), TUI (issue 624), and the TUI's oversized-space handling (issue 553).

## Verification

- **Paged read:** hundreds and thousands of local children page in MSC1772 order; while links are added and removed between pages, no row appears twice and no link present throughout is skipped; `total_children` and `next_cursor` are correct at the boundaries; the absent-`order` group pages across a boundary; a changed `order` between pages is tolerated without an error; a malformed or foreign cursor is a 400; the legacy route is unchanged.
- **Crawl, against a mock homeserver under paused tokio time:**
  - names improve page by page, with one frame per changed page and none for identical pages;
  - `MAX_PAGES` ends as `truncated`; a repeated `next_batch` ends as `pagination_loop`;
  - an expired cursor restarts once, then fails with backoff;
  - an oversized page retries at half size, later pages keep the lower limit, a crawl makes at most six size retries, and a single oversized room ends as `too_large`;
  - a failure partway keeps committed pages and resumes from the saved cursor;
  - `404`/`405 M_UNRECOGNIZED` records `unsupported`; `403` and `404 M_NOT_FOUND` record denial;
  - unlinked rooms are dropped; local links missing from the response stay listed;
  - only a complete crawl marks omitted children `not_returned`, using the returned set across a resumed crawl and resetting it on a restart;
  - an entry with a withheld field counts as returned and keeps its other fields; an `unidentified` entry disables omission for that generation;
  - a summary refreshed by another space's crawl or a room-summary job during this crawl is not marked `not_returned`;
  - completion deletes `room_discovery` rows no joined space links to;
  - a `not_returned` row reads as a non-null `discovered` with that status and no fields;
  - a field dropped by a newer page is removed;
  - a missing `encryption` field reads as unknown;
  - a panel job queued during a crawl runs before the crawl's next page.
- **Triggers:** a read without `enrich`, a read of an unjoined space, a fresh `complete` or `truncated` crawl, and a crawl in backoff all schedule nothing; an evicted `in_progress` crawl resumes from its cursor; repeated reads schedule nothing until `expires_at` or `next_attempt_at`.
- **Room summaries:** stable, unstable, and unsupported capability paths; `via` comes from the links; a room not linked from a joined space, a joined room, and an unknown room make no request.
- **Transitions:** joining a child during a page or summary request discards its result and deletes its row; a join racing the commit leaves no row visible while joined and none after the later leave; a leave of the space racing a page commit waits for it and then removes what it wrote; leaving the space during a crawl commits nothing and removes its discovery rows, including after a rejoin; removing the account during a request commits nothing; values, cursors, and backoff survive a restart while `queued` and `running` do not.
- **Live, on a disposable Synapse stack:** a space with about 300 children, some unjoined and some private, lists immediately, fills in names over several pages, and drops a child's summary after the account loses access to it.
