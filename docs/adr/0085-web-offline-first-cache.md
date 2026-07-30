# ADR 0085 — Web client offline-first content cache

## In brief

Persist the room list and each room's newest timeline page in IndexedDB, paint
them immediately as **stale**, and reconcile through the merge `refreshHead`
already implements for ADR 0061's reconnect gap-fill. Ship it in four phases,
`clients/` silo only; phase 1 involves no storage at all and stands alone.

The measured case, on a 1,752-room production account: `/v1/rooms` costs
**1,298 ms of server-side TTFB**, so the room list is blank for ~1.5 s on a
*fast desktop connection*; hydrating the same list from IndexedDB on an iPhone
costs **~1 ms**. Storage constrains nothing (41.2 GB of quota on the phone),
and no long task appears at the 1,752-row render, so the delay is removed
rather than relocated.

Two problems this ADR deliberately does **not** fix, both filed: the room-list
query recomputes every summary from the whole `events` table (#85), and API
responses are uncompressed (#86). The cache paints over both and replaces
neither.

Defaults differ by phase, because the data does: the **room-list cache is on by
default** (metadata, of a kind already persisted today) while the
**timeline-body cache is opt-in** (the first message plaintext written to disk,
and worth only tens of milliseconds). See [Privacy](#privacy).

The Context below is long because every parameter here is measured rather than
estimated. **Readers who want the design first should skip to
[Decision](#decision)**; readers checking whether the numbers support it should
read straight through.

## Context

On a slow or intermittent connection the web client shows nothing until the
network answers. Opening the app paints "Loading rooms…"; opening a room paints
"Loading timeline…"; both clear only when a request completes. This is not a
rendering cost — it is that the client keeps **no durable copy of any content
it has already seen**.

### This is not a mobile problem

The blocking resource is a network round trip, not a device: a laptop on hotel
wi-fi, a tether, or a VPN paints the same empty panes. What differs is **how
often the blank state is reached**, and there the platforms diverge sharply. A
desktop tab lives for days, so its in-memory stores stay warm and a genuine
cold start is rare — the cost a desktop user pays repeatedly is the *room
switch*, which discards a loaded timeline within a single session. A phone
browser discards backgrounded tabs aggressively, so on mobile nearly every
return to the app is a cold start.

That maps onto the rollout below: **phase 1 (in-memory, no storage) is the
desktop fix**, and phases 2-3 are what make a phone's constant cold starts
cheap. Neither platform is served by only one of them, and phase 1 is the
cheapest thing here in any case.

### What the client persists today

Every `localStorage` key the client writes:

| Key | Written by | Contents |
| --- | --- | --- |
| `axon.settings` | `stores/settings.ts:13` | user preferences |
| `axon.room_titles.v1` | `stores/rooms.ts:113` | room *display names* only |
| `axon.device_id` | `stores/device-state.ts:8` | this install's device id (ADR 0048) |
| `axon.publicRoomDirectoryServers` | `pages/RoomsIndex.tsx:26` | directory-server recents |
| `axon.token`, `axon.oauth.*` | `auth/token-paste.tsx`, `auth/oauth.tsx` | credentials |
| `axon.perf` | `perf.ts:3` | perf-readout opt-in (ADR 0077) |

That is the whole of it. The room list, room previews, unread counts, timeline
events, and decoded media are held only in memory:

- `createRoomsStore` starts at `rooms = signal([])` with `loading = true`
  (`stores/rooms.ts:126,130`), and `RoomList` gates its rows on that flag
  (`components/RoomList.tsx:788`). The title cache makes the rows *correct*
  once the list arrives; it cannot make a list appear early.
- `createTimelineStore` starts at `events = signal([])` with `loading = true`
  (`stores/timeline.ts:271-272`), and `RoomPage.tsx:1340` renders
  `Loading timeline…` until the first page resolves. There is no persistence
  anywhere in that file.
- The store is `useMemo`'d on `[api, media, accountId, roomId]`
  (`pages/RoomPage.tsx:233`), so leaving a room and returning **within one
  session** discards the events already fetched and starts over.
- `createMediaService` refcounts object URLs in a plain `Map`
  (`media/media-service.ts:202`) that dies with the page. The media routes do
  support conditional GETs — `ETag` and `If-None-Match`
  (`crates/axon-api/src/routes/media.rs:284-287,462-465`), plus `Range` and
  `Accept-Ranges` — but no route anywhere sets `Cache-Control`, so a reload
  *revalidates* rather than serving from cache: still a round trip per
  thumbnail on the connection that hurts most, and nothing at all offline.
  A further wrinkle is that the bearer-guarded proxy is fetched into a blob and
  handed to the DOM as an object URL, so the browser cache is keyed on requests
  the client issues by hand rather than on element `src`s (issue #23).
- There is no service worker: `public/` holds a `manifest.webmanifest` and
  icons, nothing else. The installed PWA has no offline shell.

### What the live data says

A read-only sweep of a **production account — 1,752 rooms, 2026-07-29** —
measured what a cache would have to hold and what it would paint over
(`scripts/cache-sizing-sweep.sh`, 40 rooms stride-sampled across the recency
order):

| | |
| --- | --- |
| Rooms | 1,752 |
| `/v1/rooms` payload | **660 KB**, 377 B/room |
| `/v1/rooms` wall time | **1.52 s** |
| Timeline page (`limit=50`) | median **17.4 KB**, mean 21.0 KB, p95 35.5 KB, max 38.3 KB |
| Per event | 806 B |
| Timeline fetch time | 6–68 ms |
| Rooms hitting the 50-event limit | **4 of 40 (10%)** |

Four things follow, one of which contradicts what an earlier draft of this ADR
predicted:

- **The room list costs 1.52 s before a single row can paint**, on a healthy
  connection, with no throttling. That is the blank window this ADR exists to
  close, and it is not bandwidth — 660 KB does not take 1.52 s to move on a
  fast link, so the bulk is the server assembling an unpaginated list of 1,752
  rooms. A client cache paints over it; nothing else the client can do will.
- **Timeline pages are cheap and fast** — 17 KB median, under 70 ms. The
  per-room cache is a nicety on a fast link and matters on a slow one; the
  room-list cache matters on *every* link. If only one phase ships, it is the
  room list.
- **90% of rooms do not fill a page.** An earlier draft predicted the opposite
  ("a real account inverts this: most rooms will fill the page") and was
  wrong: the median room in a 1,752-room account holds ~20 events, so for nine
  rooms in ten **the cached page is the room's entire history**. The
  "cached scrollback goes stale below the overlap" worry is therefore rare in
  practice, and the mean per-room record (21 KB) sits below a full page.
- **The LRU cap is now load-bearing.** Caching every room's page would be
  ~37 MB; a 30-room cap holds ~0.6 MB. On the 11-room test account the cap was
  inert, which is exactly why that account could not have settled it.

**Cache the whole room list, not a slice** — settled by the device measurement
below, which hydrates the full 660 KB in about a millisecond. An earlier draft
of this ADR asserted a slice was required, on the assumption that a
megabyte-scale record would be too expensive to read on a phone. It is not, and
the full list has a property the slice lacks: the room list is filterable, and
a filter over a partial cache would silently search 200 of 1,752 rooms.

### What the browser sees (desktop control, Chrome on Windows)

The same account, measured from the client with resource and paint timing:

| | |
| --- | --- |
| First contentful paint | 176 ms (the shell and "Loading rooms…") |
| `/v1/rooms` starts | 238 ms |
| `/v1/rooms` TTFB | **1,298 ms** |
| `/v1/rooms` total | 1,422 ms — so rows arrive at ~1.66 s |
| Transferred / decoded | **660,671 / 660,371 bytes** |
| Storage quota / usage | 10.74 GB / 2.02 MB |

Three things fall out, and two of them are not about caching:

- **91% of the room list's latency is server think-time.** TTFB is 1,298 ms of
  a 1,422 ms total, so only ~124 ms is transfer on a fast link. The 1.52 s
  measured server-side is list assembly, as suspected, and no amount of client
  work touches it. It is a query-shape problem rather than a missing `LIMIT` —
  see below and issue #85.
- **The API sends no compression.** `transferred` equals `decoded` to within
  header overhead, and there is no `CompressionLayer` in `crates/` or anything
  in `deploy/` supplying one. JSON of this shape typically compresses 8-12x, so
  660 KB should be well under 100 KB on the wire. This costs nothing on the
  desktop link measured here and is punishing on a phone — and it is a separate
  fix from pagination: compression addresses transfer, pagination addresses
  TTFB, and the cache paints over both.
- **Storage is a non-constraint on desktop.** A 10.74 GB quota against 2 MB in
  use means nothing in this ADR is quota-limited there. iOS will not look like
  this, which is why the phone capture is the one that matters.

The user-visible blank window on a *fast desktop connection* is therefore
~176 ms to ~1.66 s — about **1.5 seconds of "Loading rooms…" with no network
problem at all**, which is the clearest statement of the case for phase 2.

**And the bottleneck does not simply move.** A `longtask` observer across two
reloads reported tasks only at 105 ms / 52 ms and 50 ms / 54 ms — both during
bundle boot. Nothing appears at or after ~1.66 s, when the response lands and
1,752 rows render, so painting the list costs less than the 50 ms long-task
threshold. Removing the network wait therefore yields a real improvement rather
than trading a 1.5 s network stall for a render stall. Two limits on that
claim: `longtask` cannot see a render split into several sub-50 ms tasks, and
this is desktop — a phone at 3-5x slower might reach ~250 ms, still an order of
magnitude better than the wait it replaces.

### What the device says (iPhone, iOS 18.7 / Safari 27)

A standalone IndexedDB harness served from the client's own origin (quota is
per-origin, so this had to run there), writing synthetic records at the shapes
measured above and reading them back — median of 5:

| | |
| --- | --- |
| Storage quota | **41.2 GB** (3.08 MB in use) |
| Full room list, structured clone | **1 ms** (record: 870 KB) |
| Full room list, string + `JSON.parse` | 1 ms |
| 200-room slice, either encoding | 0–1 ms |
| One timeline page | 0 ms (22 KB) |
| Open database | 0 ms |
| Write everything (~1.5 MB) | 5–8 ms |

**The race is not close.** Hydrating the entire room list costs ~1 ms against a
1,298 ms network floor — a margin of three orders of magnitude. Three
consequences, and the first two close open questions outright:

- **Cache the whole room list, not a slice.** The slice existed only as a hedge
  against hydrate cost, and there is no hydrate cost to hedge against. Caching
  all of it keeps offline filtering complete over every room, which a slice
  would have silently made partial.
- **Storage is not a constraint on any target.** 41.2 GB on the phone against
  10.74 GB on the desktop — the phone's quota is *four times larger*. An
  earlier draft of this ADR assumed iOS would be tight and warned that its
  quota might not permit phases 2-3; that assumption was wrong, and modern iOS
  grants quota as a fraction of disk. The LRU cap survives on different
  grounds: write amplification and staleness, not space.
- **Structured clone versus stringified JSON is not decided by this data** —
  both land at the 1 ms floor. The ADR chooses structured clone anyway, because
  it keeps `JSON.parse` off the read path entirely and needs no encode step;
  but nothing here proves the other choice would be slower.

Two honest limits on these numbers. **Safari clamps `performance.now()` to
about 1 ms**, so every figure above is an upper bound at the resolution floor
rather than a precise reading — which does not weaken the conclusion, since
even the floor beats the network arm by ~1,000x. And the reads were **warm**,
taken just after the write; a genuine cold start would add a disk read that
the harness does not model. The synthetic room list also came out at 870 KB
against the 660 KB measured in production, so the test was run against a record
about 30% larger than the real one.

### The room list has a server-side problem this ADR cannot fix

`RoomsQuery` (`crates/axon-api/src/routes/rooms.rs:30-34`) accepts only
`account_id` — no `limit`, no cursor — and `list_rooms` returns every room as
one `Vec<RoomDto>`; the client calls it bare (`stores/rooms.ts:482`). At 1,752
rooms that is 660 KB assembled and returned in one shot, on every load and
every refresh, and it is where the measured 1.52 s goes.

**But the missing pagination is not what costs the 1.3 s**, and it is worth
being precise, because "add `LIMIT`" is the intuitive first move and it would
not work. `Store::list_rooms` (`crates/axon-store/src/rooms.rs:96`) derives
every room's summary from scratch on each request, aggregating the **whole
`events` table** — hundreds of thousands of rows — to produce two columns:

```sql
FROM ( SELECT account_id, room_id, MAX(origin_ts) AS last_activity_ts,
              (array_agg(event_id ORDER BY origin_ts DESC, id DESC))[1] AS last_event_id
       FROM events GROUP BY account_id, room_id ) a
```

with six correlated subqueries per room on top of it. The `GROUP BY` must
complete before `ORDER BY a.last_activity_ts DESC` can rank anything, so a
paginated first page still pays the full aggregate. The fix is to stop
recomputing per-room summaries (issue #85); pagination is worth having for
response size, and is not the latency fix.

The second server-side finding is independent: **the API sends no compression**
(issue #86), so those 660 KB cross the wire raw. Compression addresses transfer,
issue #85 addresses TTFB, and this ADR's cache paints over both without
removing either.

None of the three substitutes for another, and the cache is the weakest of them
in one specific sense: it is a paint-latency fix, not a scalability fix. Every
background refresh still pays the full 1.3 s, so a cached list can only ever be
as fresh as the last refresh that completed. This ADR should not be read as
retiring the need for issues #85 and #86.

What remains genuinely uncertain is not size but the race — does
hydrate-and-paint beat the network by enough to be worth showing stale content
— and that is a device measurement, not a server one.

So the freshness story for both principal views of server state is "fetched
once on mount, blank until it returns" — which guardrail 8 in `AGENTS.md`
("every view of server state declares its freshness story") asks us to make a
decision rather than a default. This ADR makes it one.

### The machinery this can be built on already exists

Three pieces of existing design do most of the work:

- **`refreshHead`** (`stores/timeline.ts:516`) already folds a freshly fetched
  newest page into an already-populated slice: overlapping rows win by event id
  (picking up edits and redactions missed while disconnected), older history and
  the cursor chain survive, local echoes stay at the tail, and a head that
  shares *nothing* with the loaded slice replaces it, because the gap is real.
  That is exactly the reconciliation a restored cache needs — a cache hit is
  indistinguishable, from the store's point of view, from a slice that sat idle
  while the socket was down.
- **The reconnect gap-fill contract** (ADR 0061): the live bus has no resume
  cursor, so consumers watch `live.reconnects` and re-read. Live frames are
  ingested only for the mounted room (`pages/RoomPage.tsx:435-463`), which means
  *any* store that outlives its mount — cached in memory or restored from disk —
  is already required to gap-fill on re-entry. One rule covers both.
- **The pagination cursor is stateless.** `cursor::encode`
  (`crates/axon-api/src/cursor.rs:23`) is base64url of `"{origin_ts}.{id}"`, a
  keyset over `(origin_ts, BIGSERIAL id)` — no server-side session state. See
  "Cursors are not cached" below for why we nevertheless refuse to persist one.

## Decision

Persist a bounded snapshot of the room list and of each room's newest timeline
page, render it **immediately as stale**, and reconcile it against the network
using the gap-fill merge the timeline store already implements. Ship it in four
independent phases, each its own PR, all within the `clients/` silo.

### 1. A `CacheStore` port, backed by IndexedDB

A consumer-owned port with a composition-root adapter (ADR 0021), matching how
stores already take `storage: Storage = window.localStorage` and how tests
already substitute `src/test/memory-storage.ts`:

```ts
export interface CacheStore {
  read<T>(store: CacheArea, key: string): Promise<T | undefined>
  write<T>(store: CacheArea, key: string, value: T): Promise<void>
  drop(store: CacheArea, key: string): Promise<void>
  clear(): Promise<void>
}
```

Production adapter: one IndexedDB database `axon-cache` with object stores
`rooms`, `timelines`, and `meta`. Tests get an in-memory adapter, so no
`fake-indexeddb` dependency and no jsdom IDB quirks.

IndexedDB rather than `localStorage`, on two grounds — **quota** and
**synchrony**. The room list alone is 660 KB and the timeline cache ~0.6 MB,
against a ~5 MB origin-wide `localStorage` budget shared with credentials and
settings; IndexedDB was measured at 41.2 GB of quota on the target phone. And
`localStorage` is synchronous, so every read blocks the main thread by
construction, whatever it costs.

Note that the *cost* argument this originally rested on did not survive
measurement: hydrating the full list is ~1 ms on the phone (below), so
main-thread parse expense is not why `localStorage` loses. Quota is the
disqualifying constraint; synchrony is the design objection.

**Every cache operation is best-effort.** A failed open (private browsing,
storage denied), a quota rejection, or a malformed record degrades silently to
today's behavior — the same policy `saveTitleCache` (`stores/rooms.ts:883`)
already applies, and for the same reason: a cache write must never surface an
error banner over a successful load.

### 2. Cache identity, scoping, and eviction

- Every record is keyed by `(apiBaseUrl, account_id, …)`. A client pointed at a
  different axon server, or a different account on the same server, must never
  read the other's rows.
- `meta` holds a schema version. A version mismatch drops the database rather
  than migrating it; the cache is an optimization and re-fetching is always
  correct.
- **Wipe the entire cache on any logout or token change** — not just the
  records belonging to the account that logged out. The keys are per-account
  and a surgical drop would work, but whole-cache is the conservative choice
  and the cache rebuilds itself from the next refresh at no cost beyond one
  cold start. A wipe that is too broad is invisible to the user; a wipe that
  misses something is a privacy failure. `auth/persistence.ts`'s token clear
  path gains a `cache.clear()`.
- **Request `navigator.storage.persist()`, and do not depend on it.** It is the
  only lever against a non-installed Safari tab losing script-writable storage
  after 7 idle days. Granting is heuristic and silent refusal is expected, so
  every path must behave identically whether or not it succeeded — an evicted
  cache is a cold start, which is exactly today's behavior.
- **Concurrent tabs: last writer wins, deliberately.** Two tabs share one
  database and both write back after their own refreshes. IndexedDB
  transactions serialize, so there is no torn record; and because every cached
  value is server-derived rather than user-authored, a stale overwrite costs at
  most one extra refresh. No coordination, no locks, no leader election.
- Bounded size: cap the number of rooms with a persisted timeline (propose 30,
  LRU by last-viewed) and the events per room (see below). The room list itself
  is one record. Note that this cap is about write amplification and staleness,
  not space — quota was measured at 41.2 GB on the phone and 10.7 GB on
  desktop.

### 3. Room list: cached rows, then reconcile

`createRoomsStore` gains a cache-seeded start. Because the read is async it
cannot land before the very first frame paints, but it beats the network by
~1,000x — and, as measured, on *every* connection rather than only a poor one,
because the latency it replaces is server-side.

The store gains a `stale: ReadonlySignal<boolean>` alongside `loading`.
`loading` keeps its current meaning — *nothing to show* — and becomes false as
soon as cached rows are installed; `stale` is true from that moment until the
first successful `refresh()`. `RoomList` renders cached rows with a quiet
staleness affordance instead of "Loading rooms…", and keeps the existing
message only for a genuinely cold cache.

The refresh result **replaces** the cached set wholesale rather than merging
into it, so a room left or forgotten on another client disappears — guardrail
5's prune-on-reconcile. Unread counts and previews ride along inside the cached
room DTOs and are corrected by the same refresh.

### 4. Timeline: cache the newest page only

Persist the newest `PAGE_LIMIT` (50) events per room — one page, not the
600-event `RETAINED_EVENT_LIMIT` retained slice. One page is what the room
opens on; caching the scrollback multiplies storage and staleness for content
the user has to scroll to reach anyway.

On mount, the store restores the cached page into `events`, sets
`loading = false` and `stale = true`, and issues the ordinary head fetch. The
merge is `refreshHead`'s, unchanged: overlap merges and keeps history, no
overlap replaces. A restored page whose room has been quiet is confirmed by the
first response; a restored page from a week ago shares nothing with the head
and is replaced — the exact case `refreshHead` was written for.

Write-back happens on a settled head load and on live ingestion, debounced, and
**never** includes local echoes. An echo is in-flight work, not history;
resurrecting one from disk would show a message as sent that was never sent.
(Unsent composer text is already the device-state store's job, ADR 0048.)

#### Undecryptable events may be cached, and the merge is what saves it

A cached page can contain an event that failed to decrypt. If the key arrives
later, the server's copy decrypts but the cached one does not, so a restore
would show a UTD placeholder for an event that is now readable.

This needs no special handling — but only because of a property of the merge
that is easy to mistake for an incidental detail: **`refreshHead` lets the
freshly fetched rows win by event id**, which is precisely how it already
absorbs edits and redactions missed while disconnected. A newly decryptable
event arrives through the same door. If that rule were ever weakened to "keep
what we have when the ids match", UTD placeholders would become sticky across
restarts. The rule is load-bearing; treat it as such.

Note that this makes the *restore* self-correcting but not instantaneous: the
placeholder is visible for the duration of the head fetch, exactly as stale
content is. The existing one-shot redecrypt kick (`RoomPage.tsx`) is
independent and unaffected.

#### Cursors are not cached

A restored slice carries **no cursor**. The encoding is stateless, so a
persisted cursor would decode cleanly in a later session — but its `id` half is
a `BIGSERIAL` local to that server's Postgres. A store rebuilt or re-synced
reassigns those ids, and the stale cursor then decodes *successfully* and
points at the wrong row: a silent gap spliced into history with no error
anywhere. That is the worst failure shape available to us, and the fix is
cheap, because the head fetch that immediately follows a restore supplies a
fresh cursor anyway.

This has one non-obvious consequence that the implementation must respect.
`reachedStart` is derived as `page.next === null` throughout the store
(`stores/timeline.ts:447,484,537`), so a restored slice with a null cursor
would claim it had reached the start of history and suppress `loadOlder`. The
restored state is therefore **cursor-unknown**, distinct from cursor-exhausted:
`atStart` reports false while the slice is hydrated-but-unconfirmed, and only
the settled head fetch may set it. Getting this wrong silently disables
scrollback, so it gets a dedicated test.

### 5. Media: a Cache API layer, sequenced against issue #23

Thumbnails and avatars go into a `Cache` keyed by the proxy URL (which already
encodes method and dimensions — `media/use-thumbnail-fallback.ts:28`). The
refcounted object-URL map stays exactly as it is and sits in front, so blob
lifetimes and revocation are untouched. Full-size attachment bytes are not
cached.

**This phase overlaps issue #23** ("Service worker for media streaming"), which
proposes registering a service worker that attaches the bearer token to
same-origin `/v1/media/*` requests so a media element can point straight at the
proxy. If that lands, media requests become ordinary browser-issued fetches
against a server that already speaks `ETag` and `Range` — at which point the
right media cache is the **HTTP cache**, reached through the service worker,
plus a `Cache-Control` header on the media routes; a hand-rolled Cache API
store in front of a blob path would be a second, redundant cache with its own
eviction policy.

So phase 4 is explicitly **contingent**: if issue #23 is scheduled, phase 4
should be dropped in favor of a `Cache-Control` PR on `crates/` (separate silo)
and whatever caching the service worker needs. Phase 4 stands alone only if
issue #23 stays deferred. Phases 1-3 do not depend on this either way — the
room list and timeline are JSON the client fetches with an `Authorization`
header regardless, and no service worker changes that.

### What is deliberately not cached

Search results, member lists, and ephemeral state (typing, receipts) are
either cheap to re-derive or actively misleading when stale. Local echoes are
excluded above. Nothing about verification or crypto state is cached.

## Privacy

Caching message bodies writes **plaintext to the browser profile on disk**,
including bodies that arrived from E2EE rooms — axon decrypts server-side, so
the client has never held ciphertext. On a shared machine that is a real new
exposure.

An earlier draft offered the bearer token already in `localStorage` as
precedent. That comparison does not hold and is withdrawn: **a token is
revocable and a cached conversation is not.** Revoking a token ends the access
it grants; deleting a cache does nothing about a copy already read off the
disk. Different risk classes, and the existing one does not license the new one.

### The default is split, and the measurements are why

The two caches hold different data and buy different amounts, so they get
different defaults:

- **Room-list cache: on by default.** It holds room names, topics, avatars,
  aliases, unread counts, and last-activity timestamps — metadata, not message
  text. It adds no new *category* of data at rest: the client already persists
  resolved room titles, DM titles among them, in `localStorage` under
  `axon.room_titles.v1` (`stores/rooms.ts:113`). And it is where essentially
  all of the measured benefit lives, because the 1,298 ms is the room list's.
- **Timeline-body cache: off by default, opt-in.** This is the new category of
  data at rest, and it buys the least — timeline pages fetch in 6-68 ms.
  Trading plaintext-on-disk for tens of milliseconds is a bad deal to make on a
  user's behalf; offered as a choice, particularly for offline use, it is a
  reasonable one to accept.

This split was not the original proposal. It came out of review, replacing a
single disable-everything toggle with both caches defaulting on. The practical
consequence is that **phase 2 ships without anyone having to accept
plaintext-at-rest by default**, and the decision moves to phase 3 where it can
be made deliberately.

Mitigations that apply to both: whole-cache wipe on any logout or token change;
no attachment bytes, ever; and graceful degradation wherever IDB is
unavailable, including private browsing.

**The timeline default still wants explicit sign-off rather than acceptance by
inheritance** — it is the one decision here that trades privacy for latency,
and it should be agreed to out loud.

## Alternatives considered

- **Service worker / Workbox precache.** Caches the *application shell*, not
  the data — it makes the bundle load offline while the screen stays empty, so
  it does not substitute for anything here. It is nonetheless the largest
  adjacent decision: **issue #23** already proposes registering a service
  worker for `/v1/media/*` (to authenticate ranged media requests), and its
  open questions — token rotation reaching the worker, registration and update
  lifecycle, the uncontrolled first load, whether it registers at all under the
  Tauri custom scheme in M-W12, and keeping scope off `/v1/ws` — are the same
  questions an offline shell would have to answer. If a service worker is
  going to exist, it should be designed once, in issue #23's terms, rather than
  arrived at twice. This ADR deliberately introduces none.
- **`localStorage` for timelines.** Rejected on quota and on synchronous
  main-thread parse cost, per section 1.
- **`Cache-Control` / `ETag` on the JSON `/v1/` routes.** The media routes
  already have `ETag`; extending conditional GETs to `/v1/rooms` and the
  timeline would save bytes on a repeat fetch. It does not solve this problem:
  a revalidation is still a round trip before first paint, on exactly the
  connection where the round trip is the cost, and it does nothing offline.
  Complementary, and a different silo.
- **A full local Matrix store in the browser (Element's model).** Duplicates
  what the axon server exists to be, and would need its own sync loop, crypto,
  and reconciliation. Firmly out of scope.
- **In-memory store LRU only.** Fixes room switching within a session and
  nothing on cold start — which is why it is phase 1 rather than the answer.

## Rollout

Four PRs, in order, each independently shippable:

1. **In-memory timeline-store LRU.** Replace `RoomPage`'s per-room `useMemo`
   with a bounded, account+room-keyed store cache, gap-filling via
   `refreshHead` on re-entry (required regardless: live frames only reach the
   mounted room). Fixes room switching within a session; no storage involved.

   **Measured as shipped** (desktop Chromium against the e2e mock, click to
   painted rows; the timeline GET is held open to stand in for a server's
   think-time, `/__e2e/timeline-delay`):

   | Server hold | Extra events | Cold entry | Warm re-entry (median of 6) |
   | --- | --- | --- | --- |
   | 0 ms | 0 | 25.8 ms | 11.6 ms |
   | 300 ms | 0 | 326.0 ms | 11.9 ms |
   | 300 ms | 300 | 364.8 ms | 34.6 ms |

   Two things fall out. **Cold entry tracks latency 1:1 while warm re-entry does
   not move at all** (11.6 ms against 11.9 ms across a 300 ms swing), and the
   placeholder appears on every cold entry and on no warm one — so the wait is
   removed rather than relocated, the same conclusion the long-task capture
   reached for the room list. And **what remains is render, which grows with row
   count**: 11.9 ms at ~5 rows against 34.6 ms at ~305. That is this ADR's own
   caveat arriving one phase early — a warm slice handed to an un-windowed list
   is bounded by the list (issues #26, #32), not by the cache.

   Two limits on these numbers. The mock is local, so its "cold" figure is a
   floor: the real 1.3 s room-list TTFB has no timeline equivalent measured
   here, and the 300 ms hold is a stand-in rather than a measurement. And this
   is desktop — the phone number wants the ADR 0071 harness, whose single-pane
   navigation goes through the room-list transition and so measures something
   different.
2. **`CacheStore` port + IDB adapter + room-list cache**, including the
   `stale` signal, `RoomList`'s stale affordance, a setting to disable it, and
   the logout wipe. Metadata only — no message bodies reach disk in this phase,
   so it carries none of the privacy decision.
3. **Timeline tail cache**, including the cursor-unknown state, **and the
   opt-in setting that gates it** (off by default — see Privacy). This is the
   phase that needs explicit sign-off before it ships.
4. **Media Cache API layer — only if issue #23 stays deferred** (section 5).
   Phases 1-3 are unaffected by that decision and need not wait on it.

## Testing

- Unit tests against the in-memory adapter, covering: restore-then-overlapping
  head, restore-then-disjoint head, restore-then-failed head (cached content
  must survive a failed refresh, with `stale` still true), cursor-unknown
  scrollback, echoes excluded from write-back, prune on room-list reconcile,
  and cache-key isolation across accounts and base URLs.
- One test per decision settled in section 2, since each is a rule an
  optimization could plausibly undo: a **cached UTD placeholder is replaced**
  by the merged head once the event decrypts (the win-by-id rule); **any**
  account's logout clears the **whole** cache, not just its own records; and a
  second writer's write-back leaves a readable record rather than a torn one.
- Every new test must be shown to fail against the unfixed code before it
  counts as covering anything.
- End-to-end throttled-network runs asserting content paints before the
  request settles, and offline-reload runs. `pnpm build` remains the real
  typecheck.
- Numbers come from the ADR 0071 harness and the ADR 0077 on-device readout, so
  the claim "first paint no longer waits on the network" is measured on the
  phone that prompted it, not asserted.

## Consequences

- A warm client paints content immediately and corrects it in place; the price
  is that users can now be shown stale content, which is why the staleness
  affordance is part of the decision rather than a follow-up.
- Both principal views gain an explicit freshness story: *restore cached →
  render stale → fetch head → merge by event id → prune what the server no
  longer lists*, with `live.reconnects` re-running the last three steps.
- A restore bug can show content from the wrong account or room. Cache keying
  and the logout wipe are correctness requirements with dedicated tests.
- **The two phases carry different privacy weight, and the phasing reflects it.**
  Phase 2 puts only metadata on disk, of a kind already persisted today; phase 3
  is the first time message plaintext is written, and it is opt-in. A future
  change that flips phase 3's default is a privacy decision, not a UX tweak.
- Storage grows to a bounded ceiling per origin; quota rejection is a
  no-op path, already exercised by the title cache's precedent.
- A `Cache-Control` header on the media routes becomes worthwhile either way
  (today's `ETag` support means a reload revalidates instead of hitting cache);
  it is a `crates/` PR. Whether a service worker joins it is issue #23's call,
  not this ADR's.

## Open questions

- Should the room-list cache survive a *server* change (different `apiBaseUrl`)
  as a separate namespace, or be dropped? Proposed: separate namespace, dropped
  by the LRU like anything else.
- ~~Wipe granularity, UTD events, concurrent tabs, storage persistence.~~
  **All four settled in the Decision above**: whole-cache wipe on any logout or
  token change; UTD placeholders self-correct through the merge's win-by-id
  rule, which is load-bearing rather than incidental; concurrent tabs are
  last-writer-wins because every cached value is server-derived; and
  `storage.persist()` is requested but never depended on.
- ~~Does the bottleneck just move from network wait to render?~~ **No** — the
  long-task capture above finds nothing at the room-list render.
- ~~Is 30 rooms × 50 events the right ceiling?~~ **Settled.** 50 events is not
  a free parameter at all: it is `PAGE_LIMIT`, because the restore is confirmed
  by exactly one head fetch of that size, and any surplus below the overlap
  could never be confirmed by it. At production scale the median room holds
  ~20 events anyway, so the page is usually the whole room. The room cap is a
  plain count-based LRU: 30 rooms is ~0.6 MB measured, against ~37 MB to cache
  every room, so the cap binds and byte budgeting buys nothing.
- ~~The sizing sweep needs re-running against a real account.~~ **Done**
  (1,752 rooms, above).
- ~~Whole room list or a slice?~~ **Settled: the whole list.** Hydrate is ~1 ms
  on the phone, so the slice hedged against a cost that does not exist, and the
  full list keeps offline filtering complete.
- ~~Does hydrate beat the network?~~ **Settled by ~1,000x** (iPhone measurement
  above). This was the gate on phases 2-3; it is passed.
- ~~Is storage a constraint on iOS?~~ **No** — 41.2 GB of quota, four times the
  desktop figure.
- ~~Two `crates/` issues should be filed off this work.~~ **Filed: #85** (the
  room list recomputes every summary from the whole `events` table — the real
  source of the 1,298 ms, which pagination alone would not fix) and **#86**
  (API responses are uncompressed). Both are out of scope here and neither is
  displaced by the cache.
- **Partly answered by phase 1, the rest still open:** in-app marks
  (`cache:read` → `cache:hydrate` → first painted row, in ADR 0077's
  vocabulary) plus a boot counter separating a fresh document load from a
  resumed tab. Phase 1 shipped the cheap half — `room-page:initial-load-effect`
  now carries `warm`, so how often room entry finds a warm store is readable
  from a recording — and the boot counter is still missing. The counter sets the relative value of phase 1 against phases
  2-3, and it is the one input that differs sharply between phone and desktop.
  Note also that `getEntriesByType('largest-contentful-paint')` is deprecated
  in Chrome and returns nothing; LCP needs a buffered `PerformanceObserver`.
- **Cold-start hydrate is unmeasured.** Every device figure above was taken
  warm, moments after the write. A real cold start adds a disk read the harness
  does not model. With a 1,000x margin this is very unlikely to matter, but it
  is the one number that could still surprise us.
- **Re-running the harness on a slower phone would not change anything here.**
  The margin is ~1,000x and the network arm it beats is server-side and
  therefore device-independent; a device would have to be three orders of
  magnitude slower to lose. What a slower device *would* expose is the step
  after hydrate — painting 1,752 rows — which is the room-list rendering path,
  not the cache, and is tracked separately (issues #26, #32). **A cache that
  hydrates in 1 ms and then hands an unwindowed list to a slow phone has moved
  the bottleneck, not removed it**, so phase 2 should be measured end-to-end on
  the device rather than declared finished at the hydrate boundary.
- Whether the target install is the home-screen PWA or a plain Safari tab — the
  latter loses script-writable storage after 7 idle days. Quota turned out not
  to constrain anything, but eviction policy still does, and this is unaffected
  by the 41 GB figure.
- **Needs an explicit yes, not silence: the timeline-body cache's opt-in
  default.** Review (PR #84) declined to accept a plaintext-at-rest default
  inherited from an ADR, which was the right call — the ADR now splits the
  defaults so phase 2 carries none of that weight, and phase 3 must be signed
  off before it ships. Whether opt-in is *sufficient* (as against not caching
  bodies at all, or encrypting the cache with a key held outside it) is the
  open part.
