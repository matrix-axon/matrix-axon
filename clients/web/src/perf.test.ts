import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import {
  perfMark,
  perfMarkBootRoomList,
  perfOverlayEntries,
  setPerfEnabled,
} from './perf'

/** The detail of the one `transition:back` mark, or `null` if none was made. */
function backSummary(): Record<string, unknown> | null {
  const entry = perfOverlayEntries.value.findLast(
    (candidate) => candidate.name === 'transition:back',
  )
  return entry?.detail ?? null
}

describe('back-transition summary', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    performance.clearMarks()
    perfOverlayEntries.value = []
    setPerfEnabled(true)
  })
  afterEach(() => {
    vi.useRealTimers()
    setPerfEnabled(false)
    performance.clearMarks()
  })

  /** The marks a real back-transition lays down, in order. */
  function layTransitionMarks() {
    perfMark('room-page:mobile-back', { target: 'room-list' })
    perfMark('room-list:visible-compute:start', { rooms: 150 })
    vi.advanceTimersByTime(5)
    perfMark('room-list:visible-compute:end')
    perfMark('room-list:measure:start')
    vi.advanceTimersByTime(3)
    perfMark('room-list:measure:end')
    perfMark('room-list:render')
    perfMark('room-list:post-render:now')
    vi.advanceTimersByTime(16)
    perfMark('room-list:post-render:raf2')
  }

  it('reduces a transition to the phases the e2e lane reports', () => {
    layTransitionMarks()
    expect(backSummary()).toBeNull() // not until it has settled

    vi.advanceTimersByTime(800)

    const summary = backSummary()
    // `total` runs from the gesture to the last list render; `list` is the
    // room-list phase (compute + measure) the harness attributes separately,
    // which is what distinguishes a slow list from a slow teardown.
    expect(summary).toMatchObject({
      list: 8,
      renders: 1,
      frames: 16,
      rooms: 150,
    })
    expect(summary?.total).toBeGreaterThanOrEqual(8)
  })

  it('counts every render pass, so a re-render storm is visible', () => {
    layTransitionMarks()
    perfMark('room-list:render')
    perfMark('room-list:render')
    vi.advanceTimersByTime(800)

    expect(backSummary()).toMatchObject({ renders: 3 })
  })

  it('says nothing when the gesture never reached the list', () => {
    // Closing a thread panel emits the same start mark.
    perfMark('room-page:mobile-back', { target: 'thread-close' })
    vi.advanceTimersByTime(800)

    expect(backSummary()).toBeNull()
  })

  it('emits nothing at all while instrumentation is off', () => {
    setPerfEnabled(false)
    layTransitionMarks()
    vi.advanceTimersByTime(800)

    expect(perfOverlayEntries.value).toHaveLength(0)
  })
})

/** The detail of the one `boot:room-list` mark, or `null` if none was made. */
function bootSummary(): Record<string, unknown> | null {
  const entry = perfOverlayEntries.value.findLast(
    (candidate) => candidate.name === 'boot:room-list',
  )
  return entry?.detail ?? null
}

describe('room-list boot summary (ADR 0085 phase 2)', () => {
  beforeEach(() => {
    performance.clearMarks()
    perfOverlayEntries.value = []
    setPerfEnabled(true)
  })
  afterEach(() => {
    setPerfEnabled(false)
    performance.clearMarks()
  })

  /** Run the two frames `perfMarkBootRoomList` defers across. */
  async function frames(): Promise<void> {
    for (let i = 0; i < 3; i += 1) {
      await new Promise((resolve) => requestAnimationFrame(() => resolve(null)))
    }
  }

  it('reports rows painted before the network as time saved', async () => {
    perfMark('rooms:cache:read:start')
    perfMark('rooms:cache:read:end', { hit: true, rooms: 2 })
    perfMark('rooms:hydrate', { rooms: 2 })
    perfMark('room-list:render', { hasRows: true, visible: 2 })
    perfMark('rooms:refresh:end', { ok: true, rooms: 2 })
    perfMarkBootRoomList()
    await frames()

    const summary = bootSummary()
    expect(summary).not.toBeNull()
    expect(summary!.hydrate).not.toBeNull()
    // Rows painted before the response settled, so the saving is non-negative.
    expect(summary!.saved as number).toBeGreaterThanOrEqual(0)
    expect(summary!.rooms).toBe(2)
    // The read's own duration, separate from when its result landed: `hydrate`
    // is a timestamp carrying the whole bundle boot with it, so without this a
    // slow hydrate cannot be told apart from a slow startup.
    expect(summary!.read).not.toBeNull()
    expect(summary!.boot).not.toBeNull()
  })

  it('splits startup into assets arriving and the main thread after', async () => {
    // `boot` alone cannot say whether a slow startup is a big download or slow
    // execution, and those have nothing in common as fixes. Measured on a
    // phone at 5,242 ms on a good connection, where it dwarfed the 740 ms the
    // room list then took — so every network finding was downstream of a
    // number nothing decomposed.
    const original = performance.getEntriesByType.bind(performance)
    vi.spyOn(performance, 'getEntriesByType').mockImplementation((type) => {
      if (type === 'navigation') {
        return [
          { type: 'navigate', responseEnd: 300 },
        ] as unknown as PerformanceEntryList
      }
      if (type === 'resource') {
        return [
          {
            name: 'https://a.example/assets/index-abc.js?v=1',
            responseEnd: 1200,
            transferSize: 144_000,
          },
          {
            name: 'https://a.example/assets/index-abc.css',
            responseEnd: 900,
            transferSize: 6_000,
          },
          // Not a boot asset: an API call must not be mistaken for one.
          {
            name: 'https://a.example/v1/rooms',
            responseEnd: 5_000,
            transferSize: 261_000,
          },
        ] as unknown as PerformanceEntryList
      }
      return original(type)
    })
    try {
      perfMark('rooms:cache:read:start')
      perfMark('rooms:cache:read:end', { hit: false, rooms: 0 })
      perfMark('rooms:refresh:end', { ok: true, rooms: 2 })
      perfMarkBootRoomList()
      await frames()

      const summary = bootSummary()
      expect(summary!.html).toBe(300)
      // The *last* asset the boot waited on, not the first.
      expect(summary!.js).toBe(1200)
      expect(summary!.jskb).toBe(150)
      // `exec` is main-thread time after the assets were in, so it must be
      // measured from `js` rather than from navigation start.
      expect(summary!.exec).toBe((summary!.boot as number) - 1200)
    } finally {
      vi.restoreAllMocks()
    }
  })

  it('waits for the render the refresh has not triggered yet', async () => {
    // Preact flushes after the signal write, so at the moment a refresh settles
    // the row render has not happened. Summarising inline here reported
    // `rows: null` for every cold load — the arm the cached one is compared
    // against — so the summary has to outlast the caller by a frame.
    perfMark('rooms:refresh:end', { ok: true, rooms: 2 })
    perfMarkBootRoomList()
    perfMark('room-list:render', { hasRows: true, visible: 2 })
    await frames()

    expect(bootSummary()!.rows).not.toBeNull()
  })

  it('does not count an empty render as painted rows', async () => {
    // A cold load renders the list shell before any row exists. Counting that
    // as "painted" would flatter every cold arm and make the comparison a lie.
    perfMark('room-list:render', { hasRows: false, visible: 0 })
    perfMark('rooms:refresh:end', { ok: true, rooms: 2 })
    perfMarkBootRoomList()
    await frames()

    expect(bootSummary()!.rows).toBeNull()
    expect(bootSummary()!.saved).toBeNull()
    expect(bootSummary()!.hydrate).toBeNull()
  })
})

/** The detail of the last `boot:room-open` mark, or `null` if none was made. */
function roomOpenSummary(): Record<string, unknown> | null {
  const entry = perfOverlayEntries.value.findLast(
    (candidate) => candidate.name === 'boot:room-open',
  )
  return entry?.detail ?? null
}

/** The details of every `boot:room-open:req` mark, oldest first. */
function slowRequests(): Record<string, unknown>[] {
  return perfOverlayEntries.value
    .filter((candidate) => candidate.name === 'boot:room-open:req')
    .map((candidate) => candidate.detail ?? {})
}

describe('room-open summary', () => {
  beforeEach(() => {
    performance.clearMarks()
    perfOverlayEntries.value = []
    setPerfEnabled(true)
  })
  afterEach(() => {
    setPerfEnabled(false)
    performance.clearMarks()
  })

  /** Run the two frames the settled summary defers across. */
  async function frames(): Promise<void> {
    for (let i = 0; i < 3; i += 1) {
      await new Promise((resolve) => requestAnimationFrame(() => resolve(null)))
    }
  }

  /** The room every mark in these tests belongs to, since identity now gates. */
  const ROOM = '!r:example.org'

  function openRoom(warm = false): void {
    perfMark('room-page:initial-load-effect', {
      accountId: 'a',
      roomId: ROOM,
      highlighted: false,
      warm,
    })
  }

  /**
   * The three requests `RoomPage` fires alongside the timeline page. The
   * summary waits for them before it goes out, so a test that wants a line
   * has to settle them — which is what a real room open always does.
   */
  function settleCompetitors(members = 4200): void {
    startCompetitors()
    perfMark('rooms:refresh:end', { ok: true, rooms: 2 })
    perfMark('members:refresh:end', { roomId: ROOM, members, ok: true })
    perfMark('threads:refresh:end', { roomId: ROOM, threads: 3, ok: true })
  }

  /** The three requests going out, without any of them coming back. */
  function startCompetitors(): void {
    perfMark('rooms:refresh:start')
    perfMark('members:refresh:start', { roomId: ROOM })
    perfMark('threads:refresh:start', { roomId: ROOM })
  }

  it('names the three requests the timeline page competed with', async () => {
    openRoom()
    perfMark('timeline:fetch:start', { kind: 'head', thread: false })
    settleCompetitors()
    perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
    perfMark('room-page:timeline-render', { hasRows: true, visible: 30 })
    await frames()

    const summary = roomOpenSummary()
    expect(summary).not.toBeNull()
    // The whole point of the line: when `net` lands after `list`/`members`,
    // the request that gates first paint queued behind bodies nothing on
    // screen was waiting for.
    expect(summary!.phase).toBe('settled')
    expect(summary!.net).not.toBeNull()
    expect(summary!.list).not.toBeNull()
    expect(summary!.members).not.toBeNull()
    expect(summary!.threads).not.toBeNull()
    // The member list is unpaginated, so its size is the H1 evidence.
    expect(summary!.people).toBe(4200)
    expect(summary!.attempts).toBe(1)
    expect(summary!.warm).toBe(false)
  })

  it('reports a room open that has not painted, rather than nothing at all', async () => {
    vi.useFakeTimers()
    try {
      openRoom()
      perfMark('timeline:fetch:start', { kind: 'head', thread: false })
      // The head fetch never settles — the failure this instrumentation
      // exists to describe. A summary emitted only on settle would say
      // nothing here, leaving "slow", "still going" and "instrumentation
      // broken" indistinguishable on a phone with no console.
      vi.advanceTimersByTime(10_000)

      const summary = roomOpenSummary()
      expect(summary).not.toBeNull()
      expect(summary!.phase).toBe('waiting')
      expect(summary!.net).toBeNull()
      expect(summary!.rows).toBeNull()
    } finally {
      vi.useRealTimers()
    }
  })

  it('waits for a request that settles after the paint', async () => {
    // The reading the summary exists to make possible: a competitor that
    // lands *after* the messages did is the shape that says the link was
    // saturated. An earlier version emitted on the head fetch alone and
    // reported `list: null` here — discarding the interesting case as
    // missing data, and indistinguishable from a room list never requested.
    // Caught by the e2e lane holding `/v1/rooms` open, not by jsdom.
    openRoom()
    startCompetitors()
    perfMark('timeline:fetch:start', { kind: 'head', thread: false })
    perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
    perfMark('room-page:timeline-render', { hasRows: true, visible: 30 })
    await frames()
    expect(roomOpenSummary()).toBeNull() // nothing yet: three still in flight

    settleCompetitors()
    await frames()

    const summary = roomOpenSummary()
    expect(summary).not.toBeNull()
    expect(summary!.list).not.toBeNull()
    expect(summary!.members).not.toBeNull()
    expect(summary!.threads).not.toBeNull()
    // And the paint still reads as earlier than the list it beat.
    expect(summary!.rows as number).toBeLessThanOrEqual(summary!.list as number)
  })

  it('gives up on a request that never settles rather than losing the line', async () => {
    vi.useFakeTimers()
    try {
      openRoom()
      startCompetitors()
      perfMark('rooms:refresh:end', { ok: true, rooms: 2 })
      perfMark('threads:refresh:end', { roomId: ROOM, threads: 3, ok: true })
      // Members never answers — the summary must still go out, or one hung
      // request would take the whole reading with it.
      perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
      vi.advanceTimersByTime(100) // the two frames the emit defers across
      expect(roomOpenSummary()).toBeNull()

      vi.advanceTimersByTime(3_000)

      const summary = roomOpenSummary()
      expect(summary).not.toBeNull()
      expect(summary!.phase).toBe('settled')
      expect(summary!.list).not.toBeNull()
      expect(summary!.members).toBeNull()
      // And it says *why* members is null, rather than leaving it to mean
      // either "still in flight" or "never asked for".
      expect(summary!.pending).toBe('members')
    } finally {
      vi.useRealTimers()
    }
  })

  it('counts the room-list rows that fetched a preview during the open', async () => {
    // Every rendered row asks for its message preview, and a *warm* room-list
    // cache paints rows during startup — so the same room on the same link
    // opens against a burst of per-row fetches or against none, depending only
    // on whether the cache had rows to paint. `reqs` alone cannot separate
    // those from the room's own requests.
    openRoom()
    settleCompetitors()
    for (let index = 0; index < 24; index += 1) {
      perfMark('room-row:hydrate-preview', { key: `!r${index}:hs` })
    }
    perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
    await frames()

    expect(roomOpenSummary()!.previews).toBe(24)
  })

  it("refuses a previous room's late marks", async () => {
    // Nothing cancels an in-flight request when the user leaves a room, so on
    // a slow link A's timeline and members can land after B has opened. They
    // used to be written straight into B's record, producing a line for B
    // stamped with A's timings — a capture silently attributed to the wrong
    // room, which is worse than no capture at all.
    const OTHER = '!a:example.org'
    perfMark('room-page:initial-load-effect', {
      accountId: 'a',
      roomId: OTHER,
      highlighted: false,
      warm: false,
    })
    perfMark('timeline:fetch:start', {
      kind: 'head',
      roomId: OTHER,
      thread: false,
    })

    // The user switches to B before any of A's requests come back.
    openRoom()
    settleCompetitors(7)

    // A's marks arrive late.
    perfMark('timeline:fetch:end', {
      kind: 'head',
      roomId: OTHER,
      thread: false,
      ok: true,
    })
    perfMark('members:refresh:end', {
      roomId: OTHER,
      members: 9999,
      ok: true,
    })
    perfMark('room-page:timeline-render', {
      roomId: OTHER,
      hasRows: true,
      visible: 40,
    })
    await frames()

    // None of that may settle B, and none of it may be reported as B's.
    expect(roomOpenSummary()).toBeNull()

    perfMark('timeline:fetch:start', {
      kind: 'head',
      roomId: ROOM,
      thread: false,
    })
    perfMark('timeline:fetch:end', {
      kind: 'head',
      roomId: ROOM,
      thread: false,
      ok: true,
    })
    perfMark('room-page:timeline-render', {
      roomId: ROOM,
      hasRows: true,
      visible: 3,
    })
    await frames()

    const summary = roomOpenSummary()
    expect(summary).not.toBeNull()
    expect(summary!.people).toBe(7)
    // One attempt, B's. A's `timeline:fetch:start` must not have counted.
    expect(summary!.attempts).toBe(1)
  })

  it('settles on a jump fetch, which is how a deep link fills the pane', async () => {
    // `RoomPage`'s mount effect declines to call `loadLatest` for an `?event=`
    // deep link — the jump effect owns that load, and it fetches with
    // `kind='jump'`. Anchoring on `head` alone left such an open unsettled
    // until the ten-second watchdog, so a real room open produced no line.
    openRoom()
    settleCompetitors()
    perfMark('timeline:fetch:start', { kind: 'jump', thread: false })
    perfMark('timeline:fetch:end', { kind: 'jump', thread: false, ok: true })
    perfMark('room-page:timeline-render', { hasRows: true, visible: 30 })
    await frames()

    const summary = roomOpenSummary()
    expect(summary).not.toBeNull()
    expect(summary!.via).toBe('jump')
    expect(summary!.net).not.toBeNull()
    expect(summary!.attempts).toBe(1)
  })

  it('reports a room that painted without any fetch it recognises', async () => {
    // A painted room is a finished open however it got there. Waiting out the
    // watchdog for a fetch that is never coming loses the reading entirely,
    // which is what an unrecognised entry path did.
    openRoom()
    settleCompetitors()
    perfMark('room-page:timeline-render', { hasRows: true, visible: 30 })
    await frames()

    const summary = roomOpenSummary()
    expect(summary).not.toBeNull()
    expect(summary!.via).toBe('paint')
    expect(summary!.phase).toBe('settled')
    expect(summary!.rows).not.toBeNull()
  })

  it('makes room in the resource buffer so a later open is not blind', async () => {
    // The buffer holds 250 entries and stops recording once full rather than
    // evicting. A session that has loaded a 3,600-room list exhausts it before
    // the user opens a second room, and every later open then reports
    // `reqs=0` with a good `net` — no queueing, no bytes, indistinguishable
    // from a room open that issued no requests. Measured on a phone: three
    // consecutive second-opens, all blind.
    const clear = vi.spyOn(performance, 'clearResourceTimings')
    const size = vi.spyOn(performance, 'setResourceTimingBufferSize')
    try {
      openRoom()
      expect(clear).toHaveBeenCalled()
      expect(size).toHaveBeenCalledWith(1000)
    } finally {
      vi.restoreAllMocks()
    }
  })

  it('decomposes the head fetch onto the summary line itself', async () => {
    // One screenshot from a phone has to be a complete reading, so the fields
    // that separate the hypotheses ride the summary rather than only the
    // request lines below it.
    const original = performance.getEntriesByType.bind(performance)
    try {
      openRoom()
      settleCompetitors()
      const t = performance.now()
      const room =
        '/v1/accounts/2b1e5f0a-1c3d-4e5f-8a9b-0c1d2e3f4a5b/rooms/!abc:example.org'
      const entry = (start: number, duration: number, name: string) => ({
        name,
        startTime: t + start,
        duration,
        connectStart: t + start,
        connectEnd: t + start,
        requestStart: t + start + 600,
        responseStart: t + start + 700,
        responseEnd: t + start + duration,
        transferSize: 8_000,
        decodedBodySize: 40_000,
        nextHopProtocol: 'h2',
      })
      // A *different* room's preview fetch renders as the same route and is
      // slower, so picking by duration would name the wrong request.
      const entries = [
        entry(0, 5_000, `https://a.example${room}/timeline?limit=50`),
        entry(
          0,
          9_000,
          `https://a.example/v1/accounts/x/rooms/!other:hs/timeline?limit=1`,
        ),
      ]
      vi.spyOn(performance, 'getEntriesByType').mockImplementation((type) =>
        type === 'resource'
          ? (entries as unknown as PerformanceEntryList)
          : original(type),
      )
      // Settling now makes the 5,000 ms entry the one nearest this mark.
      perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
      await frames()

      const summary = roomOpenSummary()
      expect(summary!.q).toBe(600)
      expect(summary!.ttfb).toBe(100)
      // Connection reused, so `wait` above is genuine queueing rather than a
      // handshake — the distinction that picks the fix.
      expect(summary!.conn).toBe(0)
    } finally {
      vi.restoreAllMocks()
    }
  })

  it('attributes a refresh that began before the room page mounted', async () => {
    // `App` starts the room-list refresh during boot, before `RoomPage`'s
    // mount effect, and `ensureLoaded()` coalesces onto it without marking
    // again. Watching only for a `:start` inside the open's own window
    // concluded the room list had never been requested — so a 260 KB body the
    // timeline page was competing with the whole time went unattributed, and
    // `pending` said nothing was outstanding. Found on a phone, at 3G.
    perfMark('rooms:refresh:start')
    openRoom()
    perfMark('members:refresh:start', { roomId: ROOM })
    perfMark('threads:refresh:start', { roomId: ROOM })
    perfMark('members:refresh:end', { roomId: ROOM, members: 4, ok: true })
    perfMark('threads:refresh:end', { roomId: ROOM, threads: 1, ok: true })
    perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
    await frames()

    // Still in flight, so the line must not have gone out claiming otherwise.
    expect(roomOpenSummary()).toBeNull()

    perfMark('rooms:refresh:end', { ok: true, rooms: 3638 })
    await frames()

    const summary = roomOpenSummary()
    expect(summary!.list).not.toBeNull()
    expect(summary!.pending).toBeNull()
  })

  it('tells a never-requested room list apart from one still in flight', async () => {
    // `ensureLoaded` short-circuits on a second open in one session, so no
    // room-list request goes out at all. That must not read as a room list
    // that is merely slow — on a saturated link the two look identical, and
    // reading one as the other turns a starved link into a quiet one.
    openRoom()
    perfMark('members:refresh:start', { roomId: ROOM })
    perfMark('threads:refresh:start', { roomId: ROOM })
    perfMark('members:refresh:end', { roomId: ROOM, members: 4, ok: true })
    perfMark('threads:refresh:end', { roomId: ROOM, threads: 1, ok: true })
    perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
    await frames()

    const summary = roomOpenSummary()
    // Emitted at once rather than waiting out the grace for something that was
    // never coming.
    expect(summary).not.toBeNull()
    expect(summary!.list).toBeNull()
    expect(summary!.pending).toBeNull()
  })

  it('counts repeated head fetches for one open, as a reconnect loop would', async () => {
    openRoom()
    settleCompetitors()
    perfMark('timeline:fetch:start', { kind: 'head', thread: false })
    perfMark('timeline:fetch:start', { kind: 'head', thread: false })
    perfMark('timeline:fetch:start', { kind: 'head', thread: false })
    perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
    await frames()

    expect(roomOpenSummary()!.attempts).toBe(3)
  })

  it("ignores the thread panel's own head fetch", async () => {
    openRoom()
    // A thread opens over an already-painted room, so its head load must not
    // be mistaken for the one that gates the room's first paint.
    //
    // The room's own fetch follows, so this cannot pass by measuring nothing:
    // a summary must still be emitted, and it must count one attempt rather
    // than two. Asserting only that the thread fetch produced no summary would
    // hold just as well if the instrumentation were absent entirely.
    settleCompetitors()
    perfMark('timeline:fetch:start', { kind: 'head', thread: true })
    perfMark('timeline:fetch:end', { kind: 'head', thread: true, ok: true })
    expect(roomOpenSummary()).toBeNull()

    perfMark('timeline:fetch:start', { kind: 'head', thread: false })
    perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
    await frames()

    const summary = roomOpenSummary()
    expect(summary).not.toBeNull()
    expect(summary!.attempts).toBe(1)
  })

  it('does not count an empty timeline pane as painted rows', async () => {
    // The pane renders before any event exists, exactly as the room list does.
    openRoom()
    settleCompetitors()
    perfMark('room-page:timeline-render', { hasRows: false, visible: 0 })
    perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
    await frames()

    expect(roomOpenSummary()!.rows).toBeNull()
  })

  it('reports a withheld timing breakdown as unknown, not as zero queueing', async () => {
    // `requestStart` reads 0 when a cross-origin response carries no
    // `Timing-Allow-Origin`. Reporting that as "no queueing at all" would
    // argue against the very cause the readout exists to test for.
    const original = performance.getEntriesByType.bind(performance)
    try {
      openRoom()
      settleCompetitors()
      // Built after the open starts: the readout only considers requests
      // overlapping the room open, which is what keeps an earlier page's
      // traffic out of the line.
      const entry = {
        name: 'https://axon.example.org/v1/accounts/x/rooms/y/timeline?limit=50',
        startTime: performance.now(),
        duration: 1200,
        requestStart: 0,
        responseStart: 0,
        transferSize: 0,
        decodedBodySize: 0,
        nextHopProtocol: '',
      }
      vi.spyOn(performance, 'getEntriesByType').mockImplementation((type) =>
        type === 'resource'
          ? ([entry] as unknown as PerformanceEntryList)
          : original(type),
      )
      perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
      await frames()

      const [request] = slowRequests()
      expect(request).toBeDefined()
      expect(request.cors).toBe(true)
      expect(request.wait).toBeNull()
      expect(request.ttfb).toBeNull()
      expect(request.xfer).toBeNull()
      expect(request.bytes).toBeNull()
      expect(request.gzip).toBeNull()
      expect(request.proto).toBeNull()
    } finally {
      vi.restoreAllMocks()
    }
  })

  it('counts every request that overlapped the open, not just the named three', async () => {
    // A burst of small requests — DM-title member lookups, reply targets,
    // media — can saturate a link while every individual line looks cheap.
    // `shortRoute` collapses room ids, so thirty of them render as one route;
    // the count is the only thing that shows them.
    const original = performance.getEntriesByType.bind(performance)
    try {
      openRoom()
      settleCompetitors()
      const t = performance.now()
      const entries = Array.from({ length: 12 }, (_, index) => ({
        name: `https://a.example/v1/accounts/x/rooms/!r${index}:hs/members`,
        startTime: t,
        duration: 40,
        requestStart: t + 1,
        responseStart: t + 2,
        responseEnd: t + 40,
        transferSize: 1_000,
        decodedBodySize: 5_000,
        nextHopProtocol: 'h2',
      }))
      vi.spyOn(performance, 'getEntriesByType').mockImplementation((type) =>
        type === 'resource'
          ? (entries as unknown as PerformanceEntryList)
          : original(type),
      )
      perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
      await frames()

      const summary = roomOpenSummary()
      expect(summary!.reqs).toBe(12)
      expect(summary!.kb).toBe(12)
      // Only three are named, which is exactly why the count has to exist.
      expect(slowRequests().length).toBeLessThan(12)
    } finally {
      vi.restoreAllMocks()
    }
  })

  it('names the timeline page even when three others were slower', async () => {
    // Ranking by duration alone drops the timeline request exactly when the
    // room opened well — so the good case would carry no baseline to compare
    // a bad one against. It is the request that gates the paint; it is always
    // in the line-up.
    const original = performance.getEntriesByType.bind(performance)
    try {
      openRoom()
      settleCompetitors()
      const t = performance.now()
      const at = (start: number, duration: number, name: string) => ({
        name,
        startTime: t + start,
        duration,
        requestStart: t + start + 1,
        responseStart: t + start + 2,
        responseEnd: t + start + duration,
        transferSize: 1_000,
        decodedBodySize: 5_000,
        nextHopProtocol: 'h2',
      })
      const room =
        '/v1/accounts/2b1e5f0a-1c3d-4e5f-8a9b-0c1d2e3f4a5b/rooms/!abc:example.org'
      const entries = [
        at(0, 20, `https://a.example${room}/timeline?limit=50`),
        at(0, 900, 'https://a.example/v1/rooms'),
        at(0, 800, `https://a.example${room}/members`),
        at(0, 700, `https://a.example${room}/threads`),
      ]
      vi.spyOn(performance, 'getEntriesByType').mockImplementation((type) =>
        type === 'resource'
          ? (entries as unknown as PerformanceEntryList)
          : original(type),
      )
      perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
      await frames()

      const routes = slowRequests().map((request) => request.route)
      expect(routes).toContain('accounts/{account}/rooms/{id}/timeline')
      // And the three that beat it are still named alongside.
      expect(routes).toContain('rooms')
      expect(routes).toContain('accounts/{account}/rooms/{id}/members')
    } finally {
      vi.restoreAllMocks()
    }
  })

  it('reports queueing and compression when the breakdown is exposed', async () => {
    const original = performance.getEntriesByType.bind(performance)
    try {
      openRoom()
      settleCompetitors()
      const startTime = performance.now()
      const entry = {
        name: 'https://axon.example.org/v1/accounts/2b1e5f0a-1c3d-4e5f-8a9b-0c1d2e3f4a5b/rooms/!abc:example.org/timeline?limit=50',
        startTime,
        duration: 900,
        // 800 ms of the 900 was spent queued, never on the wire — the shape
        // H1 predicts, and the one a request-path wrapper cannot see.
        requestStart: startTime + 800,
        responseStart: startTime + 850,
        responseEnd: startTime + 900,
        transferSize: 4_000,
        decodedBodySize: 40_000,
        nextHopProtocol: 'h2',
      }
      vi.spyOn(performance, 'getEntriesByType').mockImplementation((type) =>
        type === 'resource'
          ? ([entry] as unknown as PerformanceEntryList)
          : original(type),
      )
      perfMark('timeline:fetch:end', { kind: 'head', thread: false, ok: true })
      await frames()

      const [request] = slowRequests()
      expect(request.wait).toBe(800)
      expect(request.ttfb).toBe(50)
      // Transfer, which over h2 is where contention lands — there is no
      // six-request queue to lengthen `wait` on a multiplexed connection.
      expect(request.xfer).toBe(50)
      expect(request.gzip).toBe(true)
      expect(request.proto).toBe('h2')
      // Ids collapsed so the line fits a phone screen.
      expect(request.route).toBe('accounts/{account}/rooms/{id}/timeline')
    } finally {
      vi.restoreAllMocks()
    }
  })
})
