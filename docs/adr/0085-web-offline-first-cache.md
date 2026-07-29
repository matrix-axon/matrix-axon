# ADR 0085 — Web client offline-first content cache

## Context

On a slow or intermittent connection the web client shows nothing until the
network answers. Opening the app paints "Loading rooms…"; opening a room paints
"Loading timeline…"; both clear only when a request completes. This is not a
rendering cost — it is that the client keeps **no durable copy of any content
it has already seen**.

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
  (`media/media-service.ts:202`) that dies with the page, and the server sets
  no `Cache-Control` on any route, so the browser HTTP cache does not reliably
  carry thumbnails across a reload either.
- There is no service worker: `public/` holds a `manifest.webmanifest` and
  icons, nothing else. The installed PWA has no offline shell.

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

IndexedDB rather than `localStorage` because a timeline tail is tens of
kilobytes of JSON per room against a ~5 MB origin-wide `localStorage` budget
shared with credentials and settings, and because `localStorage` is
synchronous: parsing every cached room's history on the main thread at boot is
precisely the wrong trade on the low-powered phone that motivated ADR 0070 and
ADR 0071.

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
- **Wipe on logout and on token change.** This is a correctness and privacy
  requirement, not hygiene — the token clear path in `auth/persistence.ts`
  gains a `cache.clear()`.
- Bounded size: cap the number of rooms with a persisted timeline (propose 30,
  LRU by last-viewed) and the events per room (see below). The room list itself
  is one record.

### 3. Room list: cached rows, then reconcile

`createRoomsStore` gains a cache-seeded start. Because the read is async, it
cannot land before the first paint of the very first frame, but it lands far
ahead of the network on the connections this ADR is about.

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

### 5. Media: a Cache API layer under the object-URL cache

Thumbnails and avatars go into a `Cache` keyed by the proxy URL (which already
encodes method and dimensions — `media/use-thumbnail-fallback.ts:28`). The
refcounted object-URL map stays exactly as it is and sits in front, so blob
lifetimes and revocation are untouched. Full-size attachment bytes are not
cached.

Adding `Cache-Control` to `/v1/media/**` would let the HTTP cache do some of
this for free and is worth doing, but it is a `crates/` change and therefore a
separate PR under the one-silo rule. The client-side layer works regardless of
what the server sends, which is why it comes first.

### What is deliberately not cached

Search results, member lists, and ephemeral state (typing, receipts) are
either cheap to re-derive or actively misleading when stale. Local echoes are
excluded above. Nothing about verification or crypto state is cached.

## Privacy

Caching message bodies writes **plaintext to the browser profile on disk**,
including bodies that arrived from E2EE rooms — axon decrypts server-side, so
the client has never held ciphertext. On a shared machine this is a real new
exposure, even though a bearer token already sits in `localStorage`. It is
mitigated, not eliminated, by: wipe on logout and token change; a Settings
toggle to disable content caching entirely (the cache is optional by
construction, so the toggle is a no-op adapter); no attachment bytes; and
graceful degradation in private-browsing contexts where IDB is unavailable.
The toggle ships in the same phase as the first content cache, not later.

## Alternatives considered

- **Service worker / Workbox precache.** Caches the *application shell*, not
  the data — it makes the bundle load offline while the screen stays empty.
  Complementary and worth its own ADR; it also brings an update-lifecycle
  burden (stale-SW-serving-old-bundle) that should not ride along with a data
  change.
- **`localStorage` for timelines.** Rejected on quota and on synchronous
  main-thread parse cost, per section 1.
- **`Cache-Control` / `ETag` on `/v1/` responses.** Saves bytes on a repeat
  fetch but still requires a round trip before first paint, and does nothing
  offline. Complementary, different silo.
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
2. **`CacheStore` port + IDB adapter + room-list cache**, including the
   `stale` signal, `RoomList`'s stale affordance, the settings toggle, and the
   logout wipe.
3. **Timeline tail cache**, including the cursor-unknown state.
4. **Media Cache API layer.**

## Testing

- Unit tests against the in-memory adapter, covering: restore-then-overlapping
  head, restore-then-disjoint head, restore-then-failed head (cached content
  must survive a failed refresh, with `stale` still true), cursor-unknown
  scrollback, echoes excluded from write-back, prune on room-list reconcile,
  and cache-key isolation across accounts and base URLs.
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
- Storage grows to a bounded ceiling per origin; quota rejection is a
  no-op path, already exercised by the title cache's precedent.
- The server-side `Cache-Control` work and a service-worker offline shell both
  become worthwhile follow-ups, tracked separately.

## Open questions

- Should the room-list cache survive a *server* change (different `apiBaseUrl`)
  as a separate namespace, or be dropped? Proposed: separate namespace, dropped
  by the LRU like anything else.
- Is 30 rooms × 50 events the right ceiling? It should be set from the ADR 0077
  readout on a real device rather than guessed here.
