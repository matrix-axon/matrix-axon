import { effect, signal } from '@preact/signals'
import type { ApiClient } from '../api/client'
import { deviceStateChange } from '../api/frames'
import type { LiveConnection } from './live-connection'
import type { RoomDto } from './room-list'

/** localStorage key for this install's stable device id (ADR 0048). */
const DEVICE_ID_KEY = 'axon.device_id'
/** The device-state namespace drafts live under (TUI parity). */
export const DRAFTS_NAMESPACE = 'drafts'
/** The device-state namespace read markers live under (TUI parity). */
export const READ_MARKERS_NAMESPACE = 'read_markers'
/** The namespace for per-thread read positions (web unread-thread attention). */
export const THREAD_READ_MARKERS_NAMESPACE = 'thread_read_markers'
/** How long a settled edit waits before its `PUT` (one write per pause). */
const PUT_DEBOUNCE_MS = 800
/**
 * Entries per `PUT`, mirroring the server's `MAX_PUT_ENTRIES` cap in
 * `routes/device_state.rs`. Exceeding it is a `400` for the whole batch, so
 * batched writers are chunked to it rather than left to find out (#338).
 */
const MAX_PUT_ENTRIES = 64
/** Separator for composite cache keys — NUL, which never appears in ids
 *  or namespaces (same convention as `media-service.ts`). Written as the
 *  escape sequence: a raw NUL byte sat here before and rendered invisibly,
 *  which made the code look like it used a space (WCR-11). */
const SEP = '\0'

export interface DeviceStateStore {
  /** This install's stable device id (persisted; implicit-registered on PUT). */
  readonly deviceId: string
  /**
   * The current merged value for `(accountId, namespace, key)`, or `undefined`
   * when unset. Reactive — reads in a component re-render on cache changes.
   */
  get(accountId: string, namespace: string, key: string): unknown
  /**
   * Set (or clear, with `null`) a value: updates the local cache immediately,
   * then debounces a merge-`PUT`. `null` writes a tombstone (ADR 0048), so a
   * clear wins the cross-device merge instead of resurfacing a sibling's row.
   */
  set(accountId: string, namespace: string, key: string, value: unknown): void
  /** Fetch a `(accountId, namespace)` merged view once (idempotent). */
  hydrate(accountId: string, namespace: string): void
  /**
   * The same fetch, awaitable — resolves once the scope has been fetched, so a
   * writer can respect the forward-only guard instead of writing into an empty
   * cache and bypassing it.
   */
  ensureHydrated(accountId: string, namespace: string): Promise<void>
  /**
   * Send every debounced write now and resolve once the server has answered.
   * Called before an automatic reload (ADR 0087): drafts are durable, but only
   * once the `PUT` behind the 800 ms debounce has actually gone out, so without
   * this a reload lands inside the debounce window and drops the last thing the
   * user typed. Failures re-queue exactly as a debounced flush would.
   */
  flushPending(): Promise<void>
  /** Whether a `(accountId, namespace)` merged view has successfully loaded. */
  hydrated(accountId: string, namespace: string): boolean
  /**
   * Whether a `(accountId, namespace)` fetch has *finished*, successfully or
   * not.
   *
   * A caller that must not act on absent data waits for [`hydrated`]; one that
   * only needs to stop waiting uses this. The distinction is the difference
   * between a read receipt (never claim on missing evidence) and a room badge
   * (a repeating HTTP failure must not freeze it forever, since there is no
   * retry until the socket drops and reconnects).
   */
  hydrateSettled(accountId: string, namespace: string): boolean
  /**
   * How many times one `(account, namespace)` scope's entries have changed — a
   * stable scalar for a `useMemo` that reads values behind a method call and so
   * has no dependency to name. Per scope, so a draft keystroke does not
   * invalidate a memo that only reads thread markers.
   */
  revision(accountId: string, namespace: string): number

  /** Draft text for a room (`''` when none) — the `drafts` namespace. */
  draft(accountId: string, roomId: string): string
  /** Set/clear a room's draft (`''` clears); debounced like every write. */
  setDraft(accountId: string, roomId: string, text: string): void
  /** Fetch an account's drafts once — call when its rooms become reachable. */
  hydrateDrafts(accountId: string): void

  /** A room's cross-device read marker, or `null` — the `read_markers` ns. */
  readMarker(accountId: string, roomId: string): ReadMarker | null
  /**
   * Advance a room's read marker to `(eventId, originTs)` — forward only: an
   * older or equal `originTs` is ignored, so reading never moves backward
   * (TUI parity). Debounced PUT like every write.
   */
  advanceReadMarker(
    accountId: string,
    roomId: string,
    eventId: string,
    originTs: number,
  ): void
  /** Fetch an account's read markers once. */
  hydrateReadMarkers(accountId: string): void
  /** Seed missing read markers to current room summaries, without read receipts. */
  baselineReadMarkers(
    accountId: string,
    rooms: readonly RoomDto[],
  ): Promise<void>
  /** Mark current room summaries read and schedule the device-state write. */
  markRoomSummariesRead(
    accountId: string,
    rooms: readonly RoomDto[],
  ): Promise<void>

  /** A thread's read marker, or `null` — the `thread_read_markers` namespace. */
  threadReadMarker(
    accountId: string,
    roomId: string,
    rootEventId: string,
  ): ThreadReadMarker | null
  /** Advance a thread's marker forward only. */
  advanceThreadReadMarker(
    accountId: string,
    roomId: string,
    rootEventId: string,
    eventId: string,
    originTs: number,
    arrivalThrough?: number,
  ): void
  /** Fetch an account's thread read markers once. */
  hydrateThreadReadMarkers(accountId: string): void
}

/** A cross-device read position: the newest event a device has read. */
export interface ReadMarker {
  eventId: string
  originTs: number
}

/** A per-thread read position: the newest member the device has opened. */
export interface ThreadReadMarker extends ReadMarker {
  roomId: string
  rootEventId: string
  /**
   * The greatest `arrival_order` the panel had displayed when this was written,
   * or `null` for a marker written before this field existed.
   *
   * `eventId`/`originTs` are a *display* position — where the thread's "read to
   * here" line sits — and display order is not arrival order (ADR 0089). A
   * receipt decision needs the second, and resolving it by looking `eventId` up
   * in a loaded slice both understates it (a backfilled reply can be display-
   * earlier and arrival-later than the display-last event) and fails outright
   * once that event pages out. Recording it at write time removes both.
   */
  arrivalThrough: number | null
}

/**
 * What the store knows about one key's local writes: the tick of the newest
 * one, the newest tick the server has acked, and when that ack arrived.
 */
interface LocalWrite {
  written: number
  acked: number
  ackedAt: number
}

/** What is known about one `(account, namespace)` scope. */
interface ScopeState {
  /** A fetch returned data for it. */
  hydrated: boolean
  /** A fetch finished for it, with or without data. */
  settled: boolean
  /** Bumped on every change to this scope's entries. */
  revision: number
}

/** Parse a marker's wire value (`{event_id, origin_ts}`); `null` if malformed. */
function parseMarker(value: unknown): ReadMarker | null {
  if (typeof value !== 'object' || value === null) {
    return null
  }
  const { event_id: eventId, origin_ts: originTs } = value as Record<
    string,
    unknown
  >
  return typeof eventId === 'string' && typeof originTs === 'number'
    ? { eventId, originTs }
    : null
}

/** Parse a thread marker's wire value; `null` if malformed. */
export function parseThreadReadMarker(value: unknown): ThreadReadMarker | null {
  if (typeof value !== 'object' || value === null) {
    return null
  }
  const {
    room_id: roomId,
    root_event_id: rootEventId,
    event_id: eventId,
    origin_ts: originTs,
    arrival_through: arrivalThrough,
  } = value as Record<string, unknown>
  return typeof roomId === 'string' &&
    typeof rootEventId === 'string' &&
    typeof eventId === 'string' &&
    typeof originTs === 'number'
    ? {
        roomId,
        rootEventId,
        eventId,
        originTs,
        arrivalThrough:
          typeof arrivalThrough === 'number' ? arrivalThrough : null,
      }
    : null
}

/**
 * Device-state keys are opaque strings, so encode both Matrix ids into one key
 * rather than relying on a printable separator that event ids might contain.
 */
export function threadReadMarkerKey(
  roomId: string,
  rootEventId: string,
): string {
  return `${encodeURIComponent(roomId)}:${encodeURIComponent(rootEventId)}`
}

/** Read the persisted device id, minting and storing one on first run. */
function loadDeviceId(storage: Storage): string {
  const existing = storage.getItem(DEVICE_ID_KEY)
  if (existing !== null && existing !== '') {
    return existing
  }
  const id = crypto.randomUUID()
  storage.setItem(DEVICE_ID_KEY, id)
  return id
}

const cacheKey = (accountId: string, namespace: string, key: string) =>
  `${accountId}${SEP}${namespace}${SEP}${key}`
const scopeKey = (accountId: string, namespace: string) =>
  `${accountId}${SEP}${namespace}`

/**
 * Cross-device per-device state (M12, ADR 0048): the client's drafts and read
 * markers, synced through `/v1/devices/{device_id}/state/{namespace}`. State
 * is account-scoped (the `account_id` query param) and keyed by room id within
 * an account, matching the TUI so a draft typed in one client appears in the
 * other.
 *
 * The store keeps one merged LWW cache, hydrated by `GET` on demand, updated
 * locally-first with a debounced merge-`PUT`, and kept fresh by two push
 * paths: `device_state.changed` frames (echo-suppressed by our own device id)
 * and a re-`GET` of every hydrated scope on reconnect (the bus is lossy, so a
 * reconnecting client re-reads rather than trusting frames it may have missed).
 */
export function createDeviceStateStore(
  api: ApiClient,
  live: LiveConnection,
  storage: Storage = window.localStorage,
): DeviceStateStore {
  const deviceId = loadDeviceId(storage)
  const entries = signal<ReadonlyMap<string, unknown>>(new Map())
  /**
   * One record per `(account, namespace)` scope instead of a set per outcome:
   * a fetch's lifecycle is one state, and keeping `hydrated` and `settled` in
   * separate structures invites updating one and forgetting the other — which
   * is the exact bug class the two flags exist to prevent (review).
   *
   * `revision` counts writes to the scope's *entries*, a different axis from
   * the fetch lifecycle, and it is per scope on purpose: a single global
   * counter made every draft keystroke invalidate memos that only read thread
   * markers, undoing most of the memoisation it was added to enable (review).
   */
  const scopes = signal<ReadonlyMap<string, ScopeState>>(new Map())
  /** `(account, namespace)` scopes already fetched (or in flight). */
  /** In-flight or finished hydration per scope; see `ensureHydrated`. */
  const hydrations = new Map<string, Promise<void>>()
  /** Debounced pending writes per scope: `key -> value | null`. */
  const pending = new Map<string, Map<string, unknown>>()
  const timers = new Map<string, ReturnType<typeof setTimeout>>()
  /**
   * Tail of the in-flight `PUT` chain per scope, so two writes to one scope are
   * never in flight at once.
   *
   * The server merge is last-write-wins by arrival
   * (`ON CONFLICT … DO UPDATE SET value = EXCLUDED.value`, no ordering guard),
   * so overlapping PUTs are a lost update whenever the network reorders them:
   * the user types "hel", that PUT goes out, they type "hello", the second PUT
   * overtakes the first, and the first lands last and restores "hel".
   *
   * That race predates the auto-reload, but the reload is what makes it
   * unrecoverable — `flushPending` used to await only the batches it started
   * itself, so a stale PUT still in flight could land *after* the reload had
   * already destroyed the tab holding the newer text. Serializing per scope
   * removes the reordering, and `flushPending` awaits these tails rather than
   * just its own work.
   */
  const writes = new Map<string, Promise<void>>()
  /**
   * Monotonic tick, bumped on every local write and every server ack of one.
   * It orders a `GET` response against the local writes that response may not
   * reflect — see `settled`.
   */
  let clock = 0
  /**
   * Per-key local-write bookkeeping; see `settled`. One entry per distinct key
   * ever written locally, never evicted. A draft per room is bounded by joined
   * rooms; a thread read-marker per thread is not, and this tab can stay open
   * indefinitely — more so on the standalone/PWA path. Bounding it is issue
   * #368; the entries are three numbers each, so the growth is slow, not
   * absent.
   */
  const local = new Map<string, LocalWrite>()

  function noteWrite(ck: string): void {
    const rec = local.get(ck)
    if (rec === undefined) {
      local.set(ck, { written: ++clock, acked: -1, ackedAt: -1 })
    } else {
      rec.written = ++clock
    }
  }

  /** Record that the server accepted the write each key held at `sent`. */
  function noteAck(sent: ReadonlyMap<string, number>): void {
    const at = ++clock
    for (const [ck, written] of sent) {
      const rec = local.get(ck)
      // A write made while the PUT was in flight has a higher `written` tick,
      // so it stays unacked and keeps its clobber protection.
      if (rec !== undefined && written > rec.acked) {
        rec.acked = written
        rec.ackedAt = at
      }
    }
  }

  /**
   * Whether a server value fetched at tick `issued` may overwrite our cached
   * one. It may only when every local write to the key was acked *before* the
   * fetch was issued: an unacked write is a draft edit the server has not seen
   * (offline, or still debounced), and an ack that landed after the fetch was
   * issued may not be reflected in the response. Either way the local value is
   * the newer one, so the fetch must leave it alone — otherwise a reconnect
   * re-read resurrects the server's stale draft over what the user is typing.
   */
  function settled(ck: string, issued: number): boolean {
    const rec = local.get(ck)
    return (
      rec === undefined || (rec.written <= rec.acked && rec.ackedAt < issued)
    )
  }

  function writeCache(
    accountId: string,
    namespace: string,
    key: string,
    value: unknown,
  ): void {
    const next = new Map(entries.value)
    const ck = cacheKey(accountId, namespace, key)
    // Only local writes reach `writeCache`; the fetch and frame paths build
    // their maps directly, so this is the one place a write is noted.
    noteWrite(ck)
    if (value === null || value === undefined) {
      next.delete(ck)
    } else {
      next.set(ck, value)
    }
    entries.value = next
    bumpScope(accountId, namespace)
  }

  function updateScope(
    accountId: string,
    namespace: string,
    change: Partial<ScopeState>,
  ): void {
    const key = scopeKey(accountId, namespace)
    const current = scopes.value.get(key) ?? {
      hydrated: false,
      settled: false,
      revision: 0,
    }
    scopes.value = new Map(scopes.value).set(key, { ...current, ...change })
  }

  function bumpScope(accountId: string, namespace: string): void {
    const key = scopeKey(accountId, namespace)
    const current = scopes.value.get(key)
    updateScope(accountId, namespace, {
      revision: (current?.revision ?? 0) + 1,
    })
  }

  async function fetchScope(
    accountId: string,
    namespace: string,
  ): Promise<void> {
    // Best-effort: a hydrate that fails leaves the cache empty until the next
    // reconnect re-read; a rejection must not escape the fire-and-forget call
    // sites (WCR-02).
    const issued = ++clock
    let data
    try {
      ;({ data } = await api.GET('/v1/devices/{device_id}/state/{namespace}', {
        params: {
          path: { device_id: deviceId, namespace },
          query: { account_id: accountId },
        },
      }))
    } catch {
      updateScope(accountId, namespace, { settled: true })
      return
    }
    if (data === undefined) {
      updateScope(accountId, namespace, { settled: true })
      return
    }
    const next = new Map(entries.value)
    for (const [key, entry] of Object.entries(data.data.entries)) {
      const ck = cacheKey(accountId, namespace, key)
      // Never let the fetched view clobber an edit the server hasn't acked.
      if (settled(ck, issued)) {
        next.set(ck, entry.value)
      }
    }
    entries.value = next
    bumpScope(accountId, namespace)
    updateScope(accountId, namespace, { hydrated: true, settled: true })
  }

  async function putEntries(
    accountId: string,
    namespace: string,
    batch: Map<string, unknown>,
  ): Promise<void> {
    if (batch.size === 0) {
      return
    }
    const sk = scopeKey(accountId, namespace)
    const timer = timers.get(sk)
    if (timer !== undefined) {
      clearTimeout(timer)
      timers.delete(sk)
    }
    const pendingBatch = pending.get(sk)
    if (pendingBatch !== undefined) {
      for (const [key, value] of pendingBatch) {
        if (!batch.has(key)) {
          batch.set(key, value)
        }
      }
      pending.delete(sk)
    }
    await enqueuePut(accountId, namespace, batch)
  }

  /**
   * Queue a `PUT` behind whatever is already in flight for its scope, and
   * return a promise for *this* write. The single entry point for writing a
   * batch — nothing calls `putBatch` directly, or the ordering guarantee has a
   * hole in it.
   *
   * An over-cap batch is split here rather than rejected by the server: the
   * batch writers (`baselineReadMarkers`, `markRoomSummariesRead`) size their
   * work by the account's room count, so on a bridged account they sail past
   * the cap, and the 400 that came back used to be recorded as an ack (#338).
   * Chunks go out in sequence on the scope's existing chain, so a partial
   * failure still leaves the chunks that landed acked and re-queues only the
   * rest.
   */
  function enqueuePut(
    accountId: string,
    namespace: string,
    batch: Map<string, unknown>,
  ): Promise<void> {
    const sk = scopeKey(accountId, namespace)
    const chunks: Map<string, unknown>[] = []
    for (const entry of batch) {
      const last = chunks.at(-1)
      if (last === undefined || last.size >= MAX_PUT_ENTRIES) {
        chunks.push(new Map([entry]))
      } else {
        last.set(entry[0], entry[1])
      }
    }
    const next = (writes.get(sk) ?? Promise.resolve())
      // A predecessor that failed must not cancel this write: `putBatch` has
      // already re-queued its batch, and this one is the newer text.
      .catch(() => {})
      .then(async () => {
        for (const chunk of chunks) {
          await putBatch(accountId, namespace, chunk)
        }
      })
    writes.set(sk, next)
    void next
      .catch(() => {})
      .then(() => {
        // Only the current tail clears the slot; a later write has replaced it.
        if (writes.get(sk) === next) {
          writes.delete(sk)
        }
      })
    return next
  }

  function flush(accountId: string, namespace: string): void {
    const sk = scopeKey(accountId, namespace)
    const batch = pending.get(sk)
    pending.delete(sk)
    timers.delete(sk)
    if (batch === undefined || batch.size === 0) {
      return
    }
    void enqueuePut(accountId, namespace, batch)
  }

  async function putBatch(
    accountId: string,
    namespace: string,
    batch: Map<string, unknown>,
  ): Promise<void> {
    const sk = scopeKey(accountId, namespace)
    const requeue = () => {
      // The PUT did not land: put the batch back so the write isn't silently
      // lost — the local cache already shows it, so losing it would surface
      // only on the next device (WCR-12). Keys written again while the PUT was
      // in flight are newer; keep them.
      const current = pending.get(sk) ?? new Map<string, unknown>()
      for (const [key, value] of batch) {
        if (!current.has(key)) {
          current.set(key, value)
        }
      }
      pending.set(sk, current)
      if (!timers.has(sk)) {
        timers.set(
          sk,
          setTimeout(() => flush(accountId, namespace), PUT_DEBOUNCE_MS),
        )
      }
    }
    // Snapshot which local write each key holds *now*: the response acks these
    // ticks and no later one, so a keystroke during the flight stays protected.
    const sent = new Map<string, number>()
    for (const key of batch.keys()) {
      const written = local.get(cacheKey(accountId, namespace, key))?.written
      if (written !== undefined) {
        sent.set(cacheKey(accountId, namespace, key), written)
      }
    }
    // `null` entries are sent verbatim as tombstones (ADR 0048).
    let response: Response
    try {
      response = (
        await api.PUT('/v1/devices/{device_id}/state/{namespace}', {
          params: {
            path: { device_id: deviceId, namespace },
            query: { account_id: accountId },
          },
          body: { entries: Object.fromEntries(batch) },
        })
      ).response
    } catch {
      // Never got an answer (offline, a dropped connection). Always worth
      // retrying.
      requeue()
      return
    }
    // openapi-fetch *resolves* on an HTTP error, so the status has to be read
    // rather than inferred from the promise settling. Acking here regardless
    // was the bug: the server had rejected the write, and the local cache went
    // on presenting it as saved until something read the namespace back (#338).
    if (!response.ok) {
      // A 5xx or a 429 says nothing about the batch, so retry it. A 4xx is a
      // verdict — the same reasoning that has always kept this path from
      // looping on a 401 after sign-out — so the write is dropped. It stays
      // unacked either way, which keeps `settled` from letting the server's
      // older value overwrite the local one.
      if (response.status >= 500 || response.status === 429) {
        requeue()
      }
      return
    }
    noteAck(sent)
  }

  function schedulePut(
    accountId: string,
    namespace: string,
    key: string,
    value: unknown,
  ): void {
    const sk = scopeKey(accountId, namespace)
    const batch = pending.get(sk) ?? new Map<string, unknown>()
    batch.set(key, value ?? null)
    pending.set(sk, batch)
    const existing = timers.get(sk)
    if (existing !== undefined) {
      clearTimeout(existing)
    }
    timers.set(
      sk,
      setTimeout(() => flush(accountId, namespace), PUT_DEBOUNCE_MS),
    )
  }

  // Apply sibling devices' writes; drop our own echoes (ADR 0048).
  live.subscribe((frame) => {
    const change = deviceStateChange(frame)
    if (change === null || change.deviceId === deviceId) {
      return
    }
    const next = new Map(entries.value)
    for (const [key, value] of Object.entries(change.entries)) {
      const ck = cacheKey(frame.accountId, change.namespace, key)
      // A sibling's write loses to a local edit the server hasn't acked yet —
      // the same rule the fetch path applies, so a frame arriving mid-outage
      // can't erase what the user is typing either.
      const rec = local.get(ck)
      if (rec !== undefined && rec.written > rec.acked) {
        continue
      }
      if (value === null) {
        next.delete(ck)
      } else {
        next.set(ck, value)
      }
    }
    entries.value = next
    bumpScope(frame.accountId, change.namespace)
  })

  // Re-read every hydrated scope on reconnect — the lossy bus may have dropped
  // `device_state.changed` frames while disconnected (ADR 0048, ADR 0061). The
  // effect fires only when `reconnects` changes; it starts at 0 (initial run,
  // no re-read) and bumps on each reopen-after-drop.
  effect(() => {
    if (live.reconnects.value === 0) {
      return
    }
    for (const scope of hydrations.keys()) {
      const [accountId, namespace] = scope.split(SEP)
      void fetchScope(accountId, namespace)
    }
  })

  function hydrate(accountId: string, namespace: string): void {
    void ensureHydrated(accountId, namespace)
  }

  /**
   * Fetch a scope if nobody has, and resolve when that fetch has finished.
   *
   * A writer that must respect the forward-only guard has to wait for this: the
   * guard compares against the local cache, so writing into an unhydrated scope
   * skips it entirely and can regress a more advanced position — including one
   * another device already set. Hydration is otherwise only triggered by opening
   * a room, so anything wired at app startup is writing blind (review, ADR 0096).
   */
  function ensureHydrated(accountId: string, namespace: string): Promise<void> {
    const sk = scopeKey(accountId, namespace)
    const existing = hydrations.get(sk)
    if (existing !== undefined) {
      return existing
    }
    const started = fetchScope(accountId, namespace)
    hydrations.set(sk, started)
    return started
  }

  async function flushPending(): Promise<void> {
    for (const scope of [...pending.keys()]) {
      const timer = timers.get(scope)
      if (timer !== undefined) {
        clearTimeout(timer)
        timers.delete(scope)
      }
      const batch = pending.get(scope)
      pending.delete(scope)
      if (batch === undefined || batch.size === 0) {
        continue
      }
      const [accountId, namespace] = scope.split(SEP)
      void enqueuePut(accountId, namespace, batch)
    }
    // Await the *chain tails*, not just the batches queued above: a write
    // already in flight when this was called has to land before the caller
    // reloads, or it lands afterwards against a tab that no longer exists.
    // Queueing first and awaiting after means each tail is this scope's last
    // write. `allSettled` because a scope whose PUT failed has re-queued itself
    // in `putBatch`, and one failure must not hide the writes that did land.
    await Promise.allSettled([...writes.values()])
  }

  function draftText(value: unknown): string {
    return typeof value === 'object' &&
      value !== null &&
      typeof (value as { text?: unknown }).text === 'string'
      ? (value as { text: string }).text
      : ''
  }

  return {
    deviceId,
    get(accountId, namespace, key) {
      return entries.value.get(cacheKey(accountId, namespace, key))
    },
    set(accountId, namespace, key, value) {
      writeCache(accountId, namespace, key, value)
      schedulePut(accountId, namespace, key, value)
    },
    hydrate,
    ensureHydrated,
    flushPending,
    hydrated(accountId, namespace) {
      return scopes.value.get(scopeKey(accountId, namespace))?.hydrated === true
    },
    hydrateSettled(accountId, namespace) {
      return scopes.value.get(scopeKey(accountId, namespace))?.settled === true
    },
    revision(accountId, namespace) {
      return scopes.value.get(scopeKey(accountId, namespace))?.revision ?? 0
    },
    draft(accountId, roomId) {
      return draftText(
        entries.value.get(cacheKey(accountId, DRAFTS_NAMESPACE, roomId)),
      )
    },
    setDraft(accountId, roomId, text) {
      const value = text === '' ? null : { text }
      writeCache(accountId, DRAFTS_NAMESPACE, roomId, value)
      schedulePut(accountId, DRAFTS_NAMESPACE, roomId, value)
    },
    hydrateDrafts(accountId) {
      hydrate(accountId, DRAFTS_NAMESPACE)
    },
    readMarker(accountId, roomId) {
      return parseMarker(
        entries.value.get(cacheKey(accountId, READ_MARKERS_NAMESPACE, roomId)),
      )
    },
    advanceReadMarker(accountId, roomId, eventId, originTs) {
      const current = parseMarker(
        entries.value.get(cacheKey(accountId, READ_MARKERS_NAMESPACE, roomId)),
      )
      if (current !== null && current.originTs >= originTs) {
        return
      }
      const value = { event_id: eventId, origin_ts: originTs }
      writeCache(accountId, READ_MARKERS_NAMESPACE, roomId, value)
      schedulePut(accountId, READ_MARKERS_NAMESPACE, roomId, value)
    },
    hydrateReadMarkers(accountId) {
      hydrate(accountId, READ_MARKERS_NAMESPACE)
    },
    async baselineReadMarkers(accountId, rooms) {
      if (
        scopes.value.get(scopeKey(accountId, READ_MARKERS_NAMESPACE))
          ?.hydrated !== true
      ) {
        return
      }
      const batch = new Map<string, unknown>()
      for (const room of rooms) {
        if (
          room.account_id !== accountId ||
          room.last_event_id === null ||
          room.last_event_id === undefined ||
          parseMarker(
            entries.value.get(
              cacheKey(accountId, READ_MARKERS_NAMESPACE, room.room_id),
            ),
          ) !== null
        ) {
          continue
        }
        const value = {
          event_id: room.last_event_id,
          origin_ts: room.last_activity_ts,
        }
        writeCache(accountId, READ_MARKERS_NAMESPACE, room.room_id, value)
        batch.set(room.room_id, value)
      }
      await putEntries(accountId, READ_MARKERS_NAMESPACE, batch)
    },
    async markRoomSummariesRead(accountId, rooms) {
      const batch = new Map<string, unknown>()
      for (const room of rooms) {
        if (
          room.account_id !== accountId ||
          room.last_event_id === null ||
          room.last_event_id === undefined
        ) {
          continue
        }
        const current = parseMarker(
          entries.value.get(
            cacheKey(accountId, READ_MARKERS_NAMESPACE, room.room_id),
          ),
        )
        if (current !== null && current.originTs >= room.last_activity_ts) {
          continue
        }
        const value = {
          event_id: room.last_event_id,
          origin_ts: room.last_activity_ts,
        }
        writeCache(accountId, READ_MARKERS_NAMESPACE, room.room_id, value)
        batch.set(room.room_id, value)
      }
      await putEntries(accountId, READ_MARKERS_NAMESPACE, batch)
    },
    threadReadMarker(accountId, roomId, rootEventId) {
      return parseThreadReadMarker(
        entries.value.get(
          cacheKey(
            accountId,
            THREAD_READ_MARKERS_NAMESPACE,
            threadReadMarkerKey(roomId, rootEventId),
          ),
        ),
      )
    },
    advanceThreadReadMarker(
      accountId,
      roomId,
      rootEventId,
      eventId,
      originTs,
      arrivalThrough,
    ) {
      const key = threadReadMarkerKey(roomId, rootEventId)
      const current = parseThreadReadMarker(
        entries.value.get(
          cacheKey(accountId, THREAD_READ_MARKERS_NAMESPACE, key),
        ),
      )
      // Both positions move forward only, and they move independently: a
      // backfilled reply raises how far the panel has read in *arrival* order
      // while leaving the display position where it was. The display test is
      // computed once — it was written out three times, and an edit that
      // updated the guard while missing one of the ternaries would have
      // silently reintroduced a marker regression (review).
      const keepsDisplayPosition =
        current !== null && current.originTs >= originTs
      const nextArrival = Math.max(
        arrivalThrough ?? -1,
        current?.arrivalThrough ?? -1,
      )
      if (
        keepsDisplayPosition &&
        nextArrival <= (current?.arrivalThrough ?? -1)
      ) {
        return
      }
      const value = {
        room_id: roomId,
        root_event_id: rootEventId,
        event_id: keepsDisplayPosition ? current.eventId : eventId,
        origin_ts: keepsDisplayPosition ? current.originTs : originTs,
        ...(nextArrival >= 0 && { arrival_through: nextArrival }),
      }
      writeCache(accountId, THREAD_READ_MARKERS_NAMESPACE, key, value)
      schedulePut(accountId, THREAD_READ_MARKERS_NAMESPACE, key, value)
    },
    hydrateThreadReadMarkers(accountId) {
      hydrate(accountId, THREAD_READ_MARKERS_NAMESPACE)
    },
  }
}
