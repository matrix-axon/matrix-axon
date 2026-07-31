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
