import { signal } from '@preact/signals'

const STORAGE_KEY = 'axon.perf'

let enabled: boolean | null = null

/**
 * Turn instrumentation on or off from the settings store, which is the
 * ordinary way to reach it — `?perf=1` remains for the e2e harness and for a
 * one-off session. Cleared state also clears the readout, so switching it off
 * does not leave a stale tail on screen.
 */
export function setPerfEnabled(on: boolean): void {
  if (enabled === on) {
    return
  }
  enabled = on
  if (!on) {
    // Recording off means there is nothing for the readout to draw, whatever
    // its own setting says. Turning recording back on does not re-show it —
    // that is `setPerfOverlay`'s to decide.
    perfActive.value = false
  }
  try {
    if (on) {
      window.sessionStorage.setItem(STORAGE_KEY, '1')
    } else {
      window.sessionStorage.removeItem(STORAGE_KEY)
    }
  } catch {
    // Private-mode storage failures should not break the app.
  }
  if (!on) {
    perfOverlayEntries.value = []
    recentMarks = []
    // The boot breakdown is memoised against `prepareResourceBuffer`'s clear,
    // so it has to be released here too — otherwise turning instrumentation
    // off and on again reports the *previous* session's startup.
    bootAssets = null
  }
}

/**
 * Marks mirrored to the on-screen readout — the ones worth watching live.
 * Both the scroll anchor and the paging that feeds it, since a jump is usually
 * the interaction between the two; the chain's own start/end and per-fetch
 * marks are left to the full timeline, which they would otherwise crowd out.
 */
const OVERLAY_PREFIXES = [
  'timeline:anchor',
  'timeline:auto-page:fetched',
  'timeline:auto-page:stop',
  'timeline:keyboard',
  'transition:',
  'boot:',
]

/**
 * Marks that begin a "back to the room list" transition — the one ADR 0071
 * built its harness around, and the one a phone reports as slow.
 */
const TRANSITION_STARTS = new Set([
  'room-page:mobile-back',
  'room-page:swipe-right-accepted',
])
/**
 * How long after the start to read the marks back. Long enough for the list to
 * have rendered and settled on a slow device; short enough to still belong to
 * the gesture that caused it.
 */
const TRANSITION_SETTLE_MS = 800
/** How many of those to keep; enough to cover a gesture, short enough to read. */
const OVERLAY_MAX = 10

export interface PerfOverlayEntry {
  at: number
  name: string
  detail?: Record<string, string | number | boolean | null>
}

/**
 * The tail of the overlay-worthy marks. iOS Safari has no on-device console —
 * reading marks there otherwise means tethering the phone to a Mac — so the
 * numbers go on the screen instead, where a screen recording captures them in
 * the same frames as the behaviour they explain.
 */
export const perfOverlayEntries = signal<readonly PerfOverlayEntry[]>([])

/**
 * Whether the on-screen readout is drawn — **not** whether marks are recorded.
 *
 * The two were one flag, which made the overlay the price of instrumentation:
 * recording during ordinary use meant a box of numbers over the app all day.
 * Now that summaries are kept on disk and can be read back afterwards, the
 * readout is the optional half, and recording quietly in the background is the
 * useful default for catching a slow load nobody was watching for.
 */
export const perfActive = signal(false)

/**
 * Show or hide the readout. Ignored while recording is off, since there would
 * be nothing to draw.
 */
export function setPerfOverlay(on: boolean): void {
  perfActive.value = on && perfEnabled()
}

export function perfEnabled(): boolean {
  if (enabled !== null) {
    return enabled
  }
  const params = new URLSearchParams(window.location.search)
  if (params.get('perf') === '1') {
    // The URL flag is the harness and one-off-session path, and it means "show
    // me everything" — readout included, without a second setting to find.
    enabled = true
    perfActive.value = true
    try {
      window.sessionStorage.setItem(STORAGE_KEY, '1')
    } catch {
      // Private-mode storage failures should not break the app.
    }
    return true
  }
  try {
    enabled = window.sessionStorage.getItem(STORAGE_KEY) === '1'
  } catch {
    enabled = false
  }
  return enabled
}

export function perfMark(
  name: string,
  detail?: Record<string, string | number | boolean | null>,
): void {
  if (!perfEnabled()) {
    return
  }
  const markName = `axon:${name}`
  try {
    if (detail === undefined) {
      performance.mark(markName)
    } else {
      performance.mark(markName, { detail })
    }
  } catch {
    performance.mark(markName)
  }
  const at = performance.now()
  recentMarks.push({ name, t: at, detail })
  if (recentMarks.length > RECENT_MARKS_MAX) {
    recentMarks = recentMarks.slice(-RECENT_MARKS_MAX)
  }
  if (OVERLAY_PREFIXES.some((prefix) => name.startsWith(prefix))) {
    perfOverlayEntries.value = [
      ...perfOverlayEntries.value,
      { at, name, detail },
    ].slice(-OVERLAY_MAX)
  }
  if (TRANSITION_STARTS.has(name)) {
    scheduleTransitionSummary(at)
  }
  noteRoomOpen(name, at, detail)
  if (sink !== null) {
    try {
      sink(name, at, detail)
    } catch {
      // A sink must never be able to break the app it is measuring.
    }
  }
}

/**
 * Where summary marks go to be persisted, registered by the composition root.
 *
 * An inverted dependency on purpose: this module is imported by stores and by
 * `RoomList`, so it cannot reach the service graph without a cycle. The sink
 * decides for itself what it will keep — see `PERSISTED_MARKS`.
 */
type TelemetrySinkFn = (
  name: string,
  at: number,
  detail: Record<string, string | number | boolean | null> | undefined,
) => void

let sink: TelemetrySinkFn | null = null

export function setTelemetrySink(fn: TelemetrySinkFn | null): void {
  sink = fn
}

interface AxonMark {
  name: string
  t: number
  detail: unknown
}

/**
 * A bounded log of recent marks, kept alongside the `performance` timeline
 * rather than read back from it: engines cap and evict entries there, and a
 * summary that silently loses its inputs is worse than no summary. Sized to
 * comfortably span one transition.
 */
const RECENT_MARKS_MAX = 400
let recentMarks: AxonMark[] = []

/** This app's marks laid down since `from`, oldest first. */
function axonMarksSince(from: number): AxonMark[] {
  return recentMarks.filter((mark) => mark.t >= from)
}

/** Total time inside `start`/`end` pairs, ignoring an unclosed trailing one. */
function pairedTotal(marks: AxonMark[], start: string, end: string): number {
  let open: number | null = null
  let total = 0
  for (const mark of marks) {
    if (mark.name === start && open === null) {
      open = mark.t
    } else if (mark.name === end && open !== null) {
      total += mark.t - open
      open = null
    }
  }
  return total
}

function detailNumber(mark: AxonMark | undefined, key: string): number | null {
  const value = (mark?.detail as Record<string, unknown> | undefined)?.[key]
  return typeof value === 'number' ? value : null
}

let transitionTimer: ReturnType<typeof setTimeout> | null = null

function scheduleTransitionSummary(startedAt: number): void {
  if (transitionTimer !== null) {
    clearTimeout(transitionTimer)
  }
  transitionTimer = setTimeout(() => {
    transitionTimer = null
    summariseTransition(startedAt)
  }, TRANSITION_SETTLE_MS)
}

/**
 * Reduce a back-transition's marks to the same phase breakdown the e2e perf
 * lane reports (`e2e/perf-helpers.ts`), and emit it as one mark.
 *
 * The raw marks are unreadable on a phone — thousands of them, scrolling past
 * faster than a screen recording can capture — but this is four numbers that
 * sit still. It is what makes a report from a device with no console into an
 * actionable measurement, which is how this session's scrolling bug was
 * eventually pinned down.
 */
function summariseTransition(startedAt: number): void {
  const marks = axonMarksSince(startedAt)
  const renders = marks.filter((mark) => mark.name === 'room-list:render')
  if (renders.length === 0) {
    // The gesture closed a thread, or never reached the list. Nothing to say.
    return
  }
  const frames = marks.filter((mark) =>
    mark.name.startsWith('room-list:post-render'),
  )
  const firstFrame = frames.find((mark) => mark.name.endsWith(':now'))
  const lastFrame = frames.findLast((mark) => mark.name.endsWith(':raf2'))
  perfMark('transition:back', {
    total: Math.round(renders[renders.length - 1].t - startedAt),
    list: Math.round(
      pairedTotal(
        marks,
        'room-list:visible-compute:start',
        'room-list:visible-compute:end',
      ) +
        pairedTotal(marks, 'room-list:measure:start', 'room-list:measure:end'),
    ),
    renders: renders.length,
    frames:
      firstFrame === undefined || lastFrame === undefined
        ? null
        : Math.round(lastFrame.t - firstFrame.t),
    rooms: detailNumber(
      marks.find((mark) => mark.name === 'room-list:visible-compute:start'),
      'rooms',
    ),
  })
}

/**
 * Reduce this document's room-list boot to the handful of numbers that settle
 * ADR 0085 phase 2 on a real device, and emit them as one overlay mark.
 *
 * Called once per document load, when the first refresh settles — the moment
 * the race the cache exists to win is over. Every figure is milliseconds since
 * navigation start, which is what `performance.now()` already measures, so
 * these read directly against each other with no arithmetic on the phone:
 *
 * - `nav` — `navigate`, `reload`, or `back_forward`. **The cold-start
 *   counter the ADR asks for is the existence of this mark at all**: a tab
 *   resumed from the app switcher without teardown runs no new document and so
 *   emits nothing. Opens that produce a `boot:room-list` are the cold ones.
 * - `hydrate` — when cached rows reached the store; `null` for a cold cache.
 * - `rows` — when the list first rendered rows. The user-visible number.
 * - `net` — when the room-list response settled. The wait being replaced.
 * - `saved` — `net - rows`, the blank time removed. **This is the result.**
 *
 * A negative `saved` is the honest failure signal: the network beat the cache,
 * and the phase bought nothing on that load.
 */
export function perfMarkBootRoomList(): void {
  if (!perfEnabled()) {
    return
  }
  // Two frames after the caller, because the number that matters — when rows
  // were *painted* — is laid down by a render Preact has not run yet when a
  // refresh settles. Summarising inline reported `rows: null` for every cold
  // load, which is exactly the arm the cached one has to be compared against.
  requestAnimationFrame(() => requestAnimationFrame(() => summariseBoot()))
}

function summariseBoot(): void {
  const at = (name: string): number | null => {
    const mark = recentMarks.find((entry) => entry.name === name)
    return mark === undefined ? null : Math.round(mark.t)
  }
  const hydrate = at('rooms:hydrate')
  const net = at('rooms:refresh:end')
  // The first render that actually put rows on screen — an empty list renders
  // too, and reporting that as "painted" would flatter every cold load.
  const rows = recentMarks.find(
    (entry) =>
      entry.name === 'room-list:render' &&
      (entry.detail as { hasRows?: unknown } | undefined)?.hasRows === true,
  )
  const rowsAt = rows === undefined ? null : Math.round(rows.t)
  let nav: string | null = null
  try {
    const [entry] = performance.getEntriesByType('navigation')
    nav = (entry as PerformanceNavigationTiming | undefined)?.type ?? null
  } catch {
    // Not every engine exposes navigation timing; the rest still reads.
  }
  // Ordered by what a reader needs first, because the overlay is a fixed box on
  // a phone screen and the tail of a long line goes off the edge. `saved` was
  // last in the first version and was the field that got clipped — the one
  // number the whole summary exists to report.
  // How long the IndexedDB read itself took, as distinct from when its result
  // landed. `hydrate` is a *timestamp* — it carries the whole bundle boot with
  // it — so on its own it cannot say whether a slow hydrate is slow storage or
  // slow startup. This is the decomposition, and it is the one figure ADR 0085
  // lists as never having been measured on a cold start.
  const readStart = at('rooms:cache:read:start')
  const readEnd = at('rooms:cache:read:end')
  const assets = captureBootAssets()
  perfMark('boot:room-list', {
    saved: net === null || rowsAt === null ? null : net - rowsAt,
    hydrate,
    read: readStart === null || readEnd === null ? null : readEnd - readStart,
    boot: readStart,
    // `boot` decomposed. Every request the app makes waits on this, so when it
    // dominates, none of the network-side findings apply and the cache cannot
    // help — it is not even read until `boot` has elapsed.
    html: assets.html,
    stall: assets.stall,
    dns: assets.dns,
    tcp: assets.tcp,
    tls: assets.tls,
    ttfb: assets.ttfb,
    hxfer: assets.hxfer,
    js: assets.js,
    jskb: assets.bytes === null ? null : Math.round(assets.bytes / 1000),
    exec:
      readStart === null || assets.js === null ? null : readStart - assets.js,
    rows: rowsAt,
    net,
    rooms: detailNumber(
      recentMarks.find((entry) => entry.name === 'rooms:refresh:end'),
      'rooms',
    ),
    nav,
  })
}

/**
 * How long to wait before reporting a room open that has not painted yet.
 *
 * A summary that is only emitted when the head fetch *settles* cannot describe
 * the failure being investigated — a request that never settles produces no
 * line at all, and the reader cannot tell "fast" from "still going" from
 * "instrumentation broken". So a room open that is still waiting at this mark
 * reports what it has, and reports again when it finally lands.
 */
const ROOM_OPEN_WATCHDOG_MS = 10_000

/**
 * How much longer to wait, after the timeline page has landed, for the three
 * requests it was sharing the link with.
 *
 * Without this the summary is emitted the moment the head fetch settles, and
 * anything still in flight reports `null` — which silently discards **exactly
 * the case worth reading**: a competitor that settles *after* the paint is the
 * shape that says the link was saturated. A held room list reported
 * `list: null` here, indistinguishable from one that was never requested.
 */
const ROOM_OPEN_GRACE_MS = 3_000

/** How many slow requests to name per room open. The overlay is a small box. */
const ROOM_OPEN_SLOW_REQUESTS = 3

/**
 * One cold room open, filled in as it happens rather than reconstructed
 * afterwards.
 *
 * `summariseTransition` scans `recentMarks` backwards from a start time, which
 * is fine for a gesture that lasts under a second. A room open on a weak link
 * can last a minute, and `room-page:timeline-render` fires on every render
 * inside it — so a backwards scan would be racing the ring buffer's own
 * eviction for the very marks it needs. These fields are captured on the way
 * past instead, at O(1) per mark.
 *
 * Times are milliseconds **since the room open started**, not since navigation,
 * because the question here is what one room entry cost — unlike
 * `boot:room-list`, where the document load is the thing being measured.
 */
interface RoomOpen {
  start: number
  warm: boolean
  /**
   * The room this open measures, so a *previous* room's late marks cannot be
   * written into it.
   *
   * Nothing cancels an in-flight request when the user leaves a room — there
   * is no `AbortController` in any of the stores (#281) — so on a slow link,
   * switching from A to B while A's timeline, members or threads are still
   * outstanding stamped B's summary with A's timings.
   * A capture silently attributed to the wrong room is worse than no capture,
   * because the whole readout exists to be trusted.
   */
  roomId: string | null
  /** Head fetches issued for this open. More than one is the reconnect-loop signal. */
  attempts: number
  /** When the head fetch settled — the request that alone gates first paint. */
  head: number | null
  /**
   * The same instant, absolute, so the resource entry behind it can be found.
   *
   * A room open issues several requests to the *same* route shape — the room
   * list resolves previews per room, and `shortRoute` collapses room ids — so
   * three lines can read `.../rooms/{id}/timeline` and only one of them is the
   * fetch that gated the paint. Picking the slowest would have named a
   * different room's.
   */
  headAt: number | null
  /** Which path filled the pane: the head fetch, a `?event=` jump, or a paint. */
  via: 'head' | 'jump' | 'paint' | null
  /**
   * Room-list rows that asked for their message preview during this open.
   *
   * Every rendered row fires one (`RoomList`'s `hydratePreview` effect), and
   * the list paints its *cached* rows during startup — so a warm cache turns
   * a room open into a burst of per-row fetches competing with the timeline
   * page, while a cold one defers them past it entirely. Counted separately
   * from `reqs` because that is the discrimination: 8 requests and 68 are the
   * same room on the same link, differing only in whether rows had painted.
   */
  previews: number
  /** When messages first painted. The user-visible number. */
  rows: number | null
  /** When each of the three requests fired alongside it settled. */
  list: number | null
  members: number | null
  threads: number | null
  memberCount: number | null
  /**
   * Which of the three were *requested* at all.
   *
   * Without this a `null` timing is ambiguous in the worst possible way: it
   * means "never asked for" on a second in-session open (`ensureLoaded`
   * short-circuits), and "asked for and still in flight" on a saturated link —
   * opposite readings from the same character. A room list that had not come
   * back after 28 s reported `list=null`, identical to one never fetched.
   *
   * Seeded from `inFlight`, not only from a `:start` seen inside the window:
   * the room-list refresh usually begins before this page's mount effect.
   */
  listStarted: boolean
  membersStarted: boolean
  threadsStarted: boolean
  settled: boolean
  emitted: boolean
  watchdog: ReturnType<typeof setTimeout> | null
  grace: ReturnType<typeof setTimeout> | null
}

let roomOpen: RoomOpen | null = null

/**
 * Which of the three competing requests are in flight *right now*, tracked
 * globally rather than per-open.
 *
 * `App` starts the room-list refresh during boot, **before** `RoomPage`'s
 * mount effect runs — and `ensureLoaded()` then coalesces onto that in-flight
 * promise without marking again. So a room open that watched only for a
 * `:start` inside its own window concluded the room list had never been
 * requested, emitted immediately, and attributed nothing to a 260 KB body it
 * was competing with the whole time. Seeding from this at open time is what
 * makes `pending` mean what it says.
 */
const inFlight = { list: false, members: false, threads: false }

function detailOf(detail: unknown): Record<string, unknown> | undefined {
  return typeof detail === 'object' && detail !== null
    ? (detail as Record<string, unknown>)
    : undefined
}

/**
 * Track a room open across the marks that make it up.
 *
 * The room's own timeline only — a `thread: true` fetch belongs to the thread
 * panel, which opens over an already-painted room and would otherwise be
 * mistaken for the head load that gates the paint.
 */
function noteRoomOpen(
  name: string,
  at: number,
  rawDetail: Record<string, string | number | boolean | null> | undefined,
): void {
  const detail = detailOf(rawDetail)
  // Before the no-open early return: these have to be current whether or not a
  // room open is in progress, because the next one seeds itself from them.
  switch (name) {
    case 'rooms:refresh:start':
      inFlight.list = true
      break
    case 'rooms:refresh:end':
      inFlight.list = false
      break
    case 'members:refresh:start':
      inFlight.members = true
      break
    case 'members:refresh:end':
      inFlight.members = false
      break
    case 'threads:refresh:start':
      inFlight.threads = true
      break
    case 'threads:refresh:end':
      inFlight.threads = false
      break
    default:
      break
  }
  if (name === 'room-page:initial-load-effect') {
    prepareResourceBuffer()
    // A second open abandons the first: leave neither timer running, or a
    // stale grace fires into the new open's window.
    if (roomOpen !== null) {
      if (roomOpen.watchdog !== null) {
        clearTimeout(roomOpen.watchdog)
      }
      if (roomOpen.grace !== null) {
        clearTimeout(roomOpen.grace)
      }
    }
    roomOpen = {
      start: at,
      warm: detail?.warm === true,
      roomId: typeof detail?.roomId === 'string' ? detail.roomId : null,
      attempts: 0,
      head: null,
      headAt: null,
      via: null,
      previews: 0,
      rows: null,
      list: null,
      members: null,
      threads: null,
      memberCount: null,
      listStarted: inFlight.list,
      membersStarted: inFlight.members,
      threadsStarted: inFlight.threads,
      settled: false,
      emitted: false,
      grace: null,
      watchdog: setTimeout(() => {
        if (roomOpen !== null) {
          roomOpen.watchdog = null
        }
        emitRoomOpen('waiting')
      }, ROOM_OPEN_WATCHDOG_MS),
    }
    return
  }
  const open = roomOpen
  if (open === null) {
    return
  }
  // A mark that names a room must name *this* one.
  // Marks carrying no room — the cross-account room list — belong to whichever
  // open is current by definition, since there is only ever one of them.
  const marked = detail?.roomId
  if (typeof marked === 'string' && marked !== open.roomId) {
    return
  }
  const since = (): number => Math.round(at - open.start)
  // A room fills its pane on entry by one of two fetches, and anchoring on
  // `head` alone missed the other: a `?event=` deep link hands the initial
  // load to the jump effect (`RoomPage`'s mount effect declines to call
  // `loadLatest` for it), which fetches with `kind='jump'`. Such an open then
  // never settled and waited out the ten-second watchdog for no reason.
  const roomEntryFetch =
    (detail?.kind === 'head' || detail?.kind === 'jump') &&
    detail.thread !== true
  switch (name) {
    case 'timeline:fetch:start':
      if (roomEntryFetch) {
        open.attempts += 1
      }
      break
    case 'timeline:fetch:end':
      if (roomEntryFetch && !open.settled) {
        open.head = since()
        open.headAt = at
        open.via = detail.kind === 'jump' ? 'jump' : 'head'
        open.settled = true
        // Two frames, for the reason `perfMarkBootRoomList` documents: the
        // render that paints the rows has not run when the fetch settles, so
        // summarising inline reports `rows: null` on every load that worked.
        requestAnimationFrame(() =>
          requestAnimationFrame(() => readyToEmit(open)),
        )
      }
      break
    case 'room-row:hydrate-preview':
      open.previews += 1
      break
    case 'room-page:timeline-render':
      // `hasRows` and not merely "rendered": the pane renders while empty too,
      // and counting that as a paint would flatter every cold open.
      if (open.rows === null && detail?.hasRows === true) {
        open.rows = since()
        // Rows on screen with no fetch attributed: the pane filled by a path
        // this does not know about. Report it rather than wait out the
        // watchdog — a painted room is a finished open however it got there,
        // and `via=paint` says the timing below is the render, not a fetch.
        if (!open.settled) {
          open.settled = true
          open.via = 'paint'
          requestAnimationFrame(() =>
            requestAnimationFrame(() => readyToEmit(open)),
          )
        }
      }
      break
    case 'rooms:refresh:start':
      open.listStarted = true
      break
    case 'members:refresh:start':
      open.membersStarted = true
      break
    case 'threads:refresh:start':
      open.threadsStarted = true
      break
    case 'rooms:refresh:end':
      open.list ??= since()
      readyToEmit(open)
      break
    case 'members:refresh:end':
      if (open.members === null) {
        open.members = since()
        open.memberCount =
          typeof detail?.members === 'number' ? detail.members : null
      }
      readyToEmit(open)
      break
    case 'threads:refresh:end':
      open.threads ??= since()
      readyToEmit(open)
      break
    default:
      break
  }
}

/**
 * Emit once the timeline page has landed **and** the three requests beside it
 * have, or once the grace period says one of them is not going to.
 *
 * The ordering matters more than it looks: on a fast link all four settle
 * within a frame of each other and the line goes out immediately, while on a
 * saturated one the competitors trail the paint and the wait is what makes
 * that readable. Emitting on the head fetch alone would report the interesting
 * case as missing data.
 */
function readyToEmit(open: RoomOpen): void {
  if (roomOpen !== open || open.emitted || !open.settled) {
    return
  }
  // A competitor that was never requested is not something to wait for —
  // `ensureLoaded` short-circuits on a second open in one session, and waiting
  // out the grace for it would delay every such line by three seconds.
  const done = (started: boolean, at: number | null): boolean =>
    !started || at !== null
  if (
    done(open.listStarted, open.list) &&
    done(open.membersStarted, open.members) &&
    done(open.threadsStarted, open.threads)
  ) {
    emitRoomOpen('settled')
    return
  }
  open.grace ??= setTimeout(() => {
    open.grace = null
    if (roomOpen === open) {
      emitRoomOpen('settled')
    }
  }, ROOM_OPEN_GRACE_MS)
}

/**
 * Reduce a room open to one line, and name the requests it was competing with.
 *
 * `net` is the only request that gates the paint; `list`, `members` and
 * `threads` are the three fired in the same breath by `RoomPage`'s mount
 * effect. When `net` lands well after them on a slow link, the timeline page
 * was queued behind bodies nothing on screen was waiting for.
 *
 * A `waiting` phase means the head fetch had still not settled at the watchdog
 * — which is the reading this exists to make obtainable, and is not a failure
 * of the instrumentation.
 */
function emitRoomOpen(phase: 'settled' | 'waiting'): void {
  const open = roomOpen
  if (open === null || open.emitted || (phase === 'waiting' && open.settled)) {
    return
  }
  if (open.watchdog !== null) {
    clearTimeout(open.watchdog)
    open.watchdog = null
  }
  if (open.grace !== null) {
    clearTimeout(open.grace)
    open.grace = null
  }
  // Scanned once and shared. Three independent `getEntriesByType('resource')`
  // passes over up to a thousand entries, back to back, is work this readout
  // was putting on the very path it measures — and worse, `headTiming` and the
  // pinned `:req` line each chose the room's timeline request *by a different
  // rule*, so the summary and the line below it could describe two different
  // physical requests while claiming to describe one.
  const entries = resourceEntries(open.start)
  const headEntry = roomHeadEntry(open, entries)
  const load = linkLoad(entries)
  const head = headTiming(headEntry)
  perfMark('boot:room-open', {
    phase,
    via: open.via,
    rows: open.rows,
    net: open.head,
    // `net` decomposed, for the one request that gates the paint. These are
    // the fields that separate the hypotheses, and keeping them on this line
    // rather than only on the `:req` lines below is what makes a single
    // screenshot from a phone a complete reading.
    q: head.wait,
    conn: head.conn,
    ttfb: head.ttfb,
    xfer: head.xfer,
    list: open.list,
    members: open.members,
    threads: open.threads,
    people: open.memberCount,
    // What the link actually carried for this open, across *all* `/v1/`
    // traffic rather than the three named below. Without it a room open that
    // was starved by many small requests — DM-title member lookups, the
    // per-reply-target burst, media — reads as one that was not busy at all,
    // because a three-line readout cannot show thirty requests.
    reqs: load.count,
    kb: load.bytes === null ? null : Math.round(load.bytes / 1000),
    previews: open.previews,
    // Requested but not back yet at emit time. `list=null pending=list` is a
    // room list still in flight; `list=null` with no mention is one that was
    // never asked for. Reading those as the same thing turns a saturated link
    // into a quiet one.
    pending: pendingOf(open),
    attempts: open.attempts,
    warm: open.warm,
  })
  summariseRequests(entries, headEntry)
  if (phase === 'settled') {
    open.emitted = true
    roomOpen = null
  }
}

/**
 * Make room in the Resource Timing buffer for this room open's requests.
 *
 * The buffer defaults to 250 entries and **silently stops recording** once
 * full — it does not evict. A session that has loaded a 3,600-room list and
 * its per-room previews exhausts it long before the user opens a second room,
 * so every later open reported `reqs=0` with a perfectly good `net`: no
 * queueing, no bytes, no protocol, indistinguishable from a room open that
 * made no requests at all. That was the arm the measurement most needed.
 *
 * Cleared per open rather than merely enlarged, because only entries inside
 * the window are ever read, and a fixed ceiling is a bug waiting to recur on a
 * busier account.
 */
function prepareResourceBuffer(): void {
  // Before the clear below, not after: a deep link can open a room before the
  // first room-list refresh settles, and the boot assets would be gone by the
  // time `summariseBoot` looked for them.
  captureBootAssets()
  try {
    performance.setResourceTimingBufferSize?.(1000)
    // Entries for requests still in flight are added on completion, after
    // this, so nothing in the window is lost by clearing here.
    performance.clearResourceTimings?.()
  } catch {
    // Neither is load-bearing: without them the readout degrades to the empty
    // reading it already handles.
  }
}

/**
 * The room's own head fetch, matched to its resource entry by settle time.
 *
 * Route matching alone is not enough: the room list resolves a preview per
 * room, so several `.../rooms/{id}/timeline` entries can overlap one open and
 * `shortRoute` renders them identically. The head fetch is the one whose
 * `responseEnd` coincides with the `timeline:fetch:end` mark.
 */
function headTiming(entry: PerformanceResourceTiming | null): {
  wait: number | null
  conn: number | null
  ttfb: number | null
  xfer: number | null
} {
  if (entry === null) {
    return { wait: null, conn: null, ttfb: null, xfer: null }
  }
  return {
    wait: Math.round(entry.requestStart - entry.startTime),
    conn: Math.round(entry.connectEnd - entry.connectStart),
    ttfb: Math.round(entry.responseStart - entry.requestStart),
    xfer: Math.round(entry.responseEnd - entry.responseStart),
  }
}

/** Every `/v1/` request that overlapped this open, read from the buffer once. */
function resourceEntries(since: number): PerformanceResourceTiming[] {
  try {
    return performance
      .getEntriesByType('resource')
      .filter(
        (entry): entry is PerformanceResourceTiming =>
          entry.startTime >= since && entry.name.includes('/v1/'),
      )
  } catch {
    // Not every engine exposes resource timing; the summary still reads.
    return []
  }
}

/**
 * The room's own head fetch, matched to its resource entry by settle time.
 *
 * **The one place this is decided.** Route matching alone is not enough: the
 * room list resolves a preview per room, so several `.../rooms/{id}/timeline`
 * entries can overlap one open and `shortRoute` renders them identically.
 * Choosing by duration instead — which the pinned `:req` line used to do —
 * picks whichever was slowest, which is routinely a different room's preview,
 * and then the summary and the line beneath it describe two different physical
 * requests while claiming to describe one.
 */
function roomHeadEntry(
  open: RoomOpen,
  entries: readonly PerformanceResourceTiming[],
): PerformanceResourceTiming | null {
  if (open.headAt === null) {
    return null
  }
  const headAt = open.headAt
  let best: PerformanceResourceTiming | null = null
  let bestGap = Number.POSITIVE_INFINITY
  for (const entry of entries) {
    // A withheld breakdown reads 0 across the board, which would win the
    // nearest-settle match with meaningless numbers.
    if (!roomTimelinePage(entry.name) || entry.requestStart <= 0) {
      continue
    }
    const gap = Math.abs(entry.responseEnd - headAt)
    if (gap < bestGap) {
      best = entry
      bestGap = gap
    }
  }
  return best
}

/** The requested-but-unsettled competitors at emit time, or `null` if none. */
function pendingOf(open: RoomOpen): string | null {
  const names = [
    open.listStarted && open.list === null ? 'list' : null,
    open.membersStarted && open.members === null ? 'members' : null,
    open.threadsStarted && open.threads === null ? 'threads' : null,
  ].filter((name): name is string => name !== null)
  return names.length === 0 ? null : names.join('+')
}

/**
 * How much `/v1/` traffic overlapped a room open, in requests and bytes.
 *
 * Deliberately counted rather than inferred from the three named requests:
 * `shortRoute` collapses room ids, so two rooms' member lookups render
 * identically and a burst of them is invisible. `bytes` is `null` when no
 * entry exposed a transfer size, which is not the same as zero.
 */
function linkLoad(entries: readonly PerformanceResourceTiming[]): {
  count: number
  bytes: number | null
} {
  const sized = entries.filter((entry) => entry.transferSize > 0)
  return {
    count: entries.length,
    bytes:
      sized.length === 0
        ? null
        : sized.reduce((total, entry) => total + entry.transferSize, 0),
  }
}

/**
 * The slowest `/v1/` requests overlapping a room open, from Resource Timing.
 *
 * Read from the browser rather than instrumented in `api/client.ts`, because
 * this gives the one number a request-path wrapper cannot: **`wait`**, the gap
 * between the fetch being started and its bytes going out. That is queueing —
 * connection-limited, or bandwidth-starved by a request already in flight — and
 * it is what separates "the server was slow" from "we asked for four things at
 * once on a link that fits one".
 *
 * `wait` and `ttfb` are `null` when the detailed fields are not exposed: they
 * require the resource to be same-origin, or to carry `Timing-Allow-Origin`.
 * A cross-origin deployment reports `cors: true` and gives up only the
 * breakdown, not the total.
 */
function summariseRequests(
  entries: readonly PerformanceResourceTiming[],
  head: PerformanceResourceTiming | null,
): void {
  // The head fetch is pinned in whether or not it ranks, because it is the one
  // request that gates the paint and it is *fastest* exactly when the room
  // opens well — so ranking by duration alone drops the baseline reading and
  // keeps only the requests that beat it.
  // It is the entry `roomHeadEntry` chose, not a second guess at which request
  // that was: picking it again here by duration is what let this line and the
  // summary above disagree about the same request.
  const byDuration = [...entries].sort((a, b) => b.duration - a.duration)
  const shown = [
    ...(head === null ? [] : [head]),
    ...byDuration.filter((entry) => entry !== head),
  ].slice(0, ROOM_OPEN_SLOW_REQUESTS + (head === null ? 0 : 1))
  for (const entry of shown) {
    // `requestStart` reads 0 when timing detail is withheld, and reporting that
    // as "no queueing at all" would argue against the very cause being tested.
    const detailed = entry.requestStart > 0
    perfMark('boot:room-open:req', {
      route: shortRoute(entry.name),
      total: Math.round(entry.duration),
      wait: detailed ? Math.round(entry.requestStart - entry.startTime) : null,
      ttfb: detailed
        ? Math.round(entry.responseStart - entry.requestStart)
        : null,
      // Connection setup inside `wait`. Zero on a reused connection, which is
      // what makes the rest of `wait` genuine queueing rather than a handshake
      // — the difference between "queued behind other requests" and "opening a
      // socket on a slow link", which need different fixes.
      conn: detailed ? Math.round(entry.connectEnd - entry.connectStart) : null,
      // The transfer phase, and over HTTP/2 the number that matters most.
      // With one multiplexed connection there is no six-request queue to show
      // up as `wait`; competing bodies interleave on the wire instead, so
      // contention lengthens *this* rather than the queueing ahead of it.
      xfer: detailed
        ? Math.round(entry.responseEnd - entry.responseStart)
        : null,
      // `transferSize` is 0 for a cache hit *and* when withheld, so it cannot
      // be reported as a byte count on its own.
      bytes: entry.transferSize > 0 ? entry.transferSize : null,
      // Whether #231's compression actually survived to this client, or a
      // proxy in front of the server stripped it.
      gzip:
        entry.transferSize > 0 && entry.decodedBodySize > 0
          ? entry.transferSize < entry.decodedBodySize
          : null,
      proto: entry.nextHopProtocol === '' ? null : entry.nextHopProtocol,
      cors: !detailed,
    })
  }
}

/** Whether a resource name is a room's timeline page (thread timelines included). */
function timelinePage(url: string): boolean {
  return url.includes('/timeline')
}

/** A *room's* timeline page — a thread's own timeline is a different request. */
function roomTimelinePage(url: string): boolean {
  return timelinePage(url) && !url.includes('/threads/')
}

/** A route shape short enough to read on a phone: ids collapsed, query dropped. */
function shortRoute(url: string): string {
  let path = url
  try {
    path = new URL(url, window.location.href).pathname
  } catch {
    // A relative or malformed name: fall back to the raw string.
  }
  return path
    .replace(
      /\/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi,
      '/{account}',
    )
    .replace(/\/[!@$][^/]+/g, '/{id}')
    .replace(/^\/v1\//, '')
}

/**
 * When the documents and code the app boots from finished arriving.
 *
 * Captured **once** and memoised, because `prepareResourceBuffer` clears
 * resource timings at each room open: a deep link that opens a room before the
 * first room-list refresh settles would otherwise wipe the boot assets before
 * anything read them.
 *
 * - `html` — the navigation response, i.e. when the document itself was in.
 * - `js` — the last script or stylesheet the boot needed. `boot - js` is
 *   therefore parse, execute, first render and service construction: main
 *   thread, not network.
 * - `bytes` — what those assets actually cost on the wire. Near zero means
 *   they came from the HTTP cache, which makes a large `exec` unambiguous;
 *   a large figure means a genuine cold download and the split matters.
 *
 * A home-screen PWA has its own HTTP cache and its own storage, separate from
 * Safari's, so its first launch is cold on both counts however much the same
 * site has been used in the browser.
 */
interface BootAssets {
  html: number | null
  js: number | null
  bytes: number | null
  /** The document fetch decomposed — see `documentPhases`. */
  stall: number | null
  dns: number | null
  tcp: number | null
  tls: number | null
  ttfb: number | null
  hxfer: number | null
}

let bootAssets: BootAssets | null = null

function captureBootAssets(): BootAssets {
  if (bootAssets !== null) {
    return bootAssets
  }
  let html: number | null = null
  let js: number | null = null
  let bytes: number | null = null
  let phases = documentPhases(undefined)
  try {
    const [navigation] = performance.getEntriesByType('navigation')
    const entry = navigation as PerformanceNavigationTiming | undefined
    const responseEnd = entry?.responseEnd
    html = typeof responseEnd === 'number' ? Math.round(responseEnd) : null
    phases = documentPhases(entry)
    const boot = performance
      .getEntriesByType('resource')
      .filter((entry): entry is PerformanceResourceTiming =>
        bootAsset((entry as PerformanceResourceTiming).name),
      )
    if (boot.length > 0) {
      js = Math.round(
        boot.reduce((latest, entry) => Math.max(latest, entry.responseEnd), 0),
      )
      bytes = boot.reduce((total, entry) => total + entry.transferSize, 0)
    }
  } catch {
    // No navigation or resource timing; the rest of the summary still reads.
  }
  bootAssets = { html, js, bytes, ...phases }
  return bootAssets
}

/**
 * The document fetch, broken into the phases that can dominate it.
 *
 * `html` on its own says the document took 41 seconds and not why — the same
 * shape of gap `boot` had before it was decomposed. Each of these fails for a
 * different reason and has a different fix, so they are worth separating:
 *
 * - `stall` — navigation start to the first DNS work. A sleeping cell radio
 *   negotiating its way back onto the network lands here, and nothing the app
 *   or the server does can shorten it.
 * - `dns`, `tcp`, `tls` — name resolution and connection setup. A protocol
 *   negotiation that has to time out and retry (an HTTP/3 attempt on a link
 *   where UDP is degraded, falling back to TCP) shows up in these two rather
 *   than in the request itself.
 * - `ttfb` — the server's own think-time, which the room-list and room-open
 *   figures alongside can be compared against: fast requests after a slow
 *   document mean the server was never the problem.
 * - `hxfer` — moving the document's bytes, which for a small HTML shell should
 *   be negligible on any link that is working at all.
 */
function documentPhases(entry: PerformanceNavigationTiming | undefined): {
  stall: number | null
  dns: number | null
  tcp: number | null
  tls: number | null
  ttfb: number | null
  hxfer: number | null
} {
  if (entry === undefined) {
    return {
      stall: null,
      dns: null,
      tcp: null,
      tls: null,
      ttfb: null,
      hxfer: null,
    }
  }
  const span = (from: number, to: number): number | null =>
    // A phase that did not happen reports both marks as zero — a reused
    // connection has no DNS or handshake — and reporting that as a real 0 is
    // right, but a *partial* entry must not turn into a negative span.
    typeof from === 'number' && typeof to === 'number' && to >= from
      ? Math.round(to - from)
      : null
  return {
    stall: span(entry.fetchStart, entry.domainLookupStart),
    dns: span(entry.domainLookupStart, entry.domainLookupEnd),
    tcp: span(entry.connectStart, entry.connectEnd),
    // Zero when the handshake was not part of this connection.
    tls:
      entry.secureConnectionStart > 0
        ? span(entry.secureConnectionStart, entry.connectEnd)
        : null,
    ttfb: span(entry.requestStart, entry.responseStart),
    hxfer: span(entry.responseStart, entry.responseEnd),
  }
}

/** Scripts and stylesheets, which are what the boot actually waits on. */
function bootAsset(url: string): boolean {
  const path = url.split('?')[0]
  return path.endsWith('.js') || path.endsWith('.css')
}

export function perfMarkFrames(name: string): void {
  if (!perfEnabled()) {
    return
  }
  perfMark(`${name}:now`)
  requestAnimationFrame(() => {
    perfMark(`${name}:raf1`)
    requestAnimationFrame(() => perfMark(`${name}:raf2`))
  })
}
