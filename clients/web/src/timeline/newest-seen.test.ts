import { afterEach, describe, expect, it, vi } from 'vitest'
import { anchorCaughtUp, observeNewestSeen } from './newest-seen'

/** One frame, so a pending `requestAnimationFrame` wait can advance. */
const frame = () =>
  new Promise<void>((resolve) => requestAnimationFrame(() => resolve()))

/**
 * Records the observer's callback so a test can play "the marker came into
 * view". jsdom rects are all zero, so the geometry re-check always reads "on
 * screen" and the waiting logic is the thing under test.
 */
function installIntersectionObserver() {
  let callback: IntersectionObserverCallback | null = null
  const disconnect = vi.fn()
  class FakeIntersectionObserver {
    constructor(cb: IntersectionObserverCallback) {
      callback = cb
    }
    observe() {}
    disconnect = disconnect
    unobserve() {}
    takeRecords() {
      return []
    }
  }
  vi.stubGlobal('IntersectionObserver', FakeIntersectionObserver)
  return {
    disconnect,
    fire: (isIntersecting: boolean) =>
      callback?.(
        [{ isIntersecting } as IntersectionObserverEntry],
        null as unknown as IntersectionObserver,
      ),
  }
}

function mounted() {
  const root = document.createElement('div')
  const marker = document.createElement('div')
  root.append(marker)
  document.body.append(root)
  return { root, marker }
}

afterEach(() => {
  vi.unstubAllGlobals()
  document.body.replaceChildren()
})

describe('observeNewestSeen', () => {
  it('reports at once when the reveal has already run', () => {
    const io = installIntersectionObserver()
    const { root, marker } = mounted()
    const seen = vi.fn()
    observeNewestSeen(marker, root, () => true, seen)

    io.fire(true)

    expect(seen).toHaveBeenCalledTimes(1)
  })

  it('waits for a reveal that finishes after the first report (PR #1145 review)', async () => {
    // The observer calls back on a *change* only: a marker already on screen
    // gets one callback, and dropping it because the reveal was still pending
    // meant no second one ever came.
    const io = installIntersectionObserver()
    const { root, marker } = mounted()
    let revealed = false
    const seen = vi.fn()
    observeNewestSeen(marker, root, () => revealed, seen)

    io.fire(true)
    expect(seen).not.toHaveBeenCalled()

    await frame()
    revealed = true
    await frame()
    await frame()

    expect(seen).toHaveBeenCalledTimes(1)
  })

  it('reports nothing when the reveal never comes', async () => {
    const io = installIntersectionObserver()
    const { root, marker } = mounted()
    const seen = vi.fn()
    observeNewestSeen(marker, root, () => false, seen)

    io.fire(true)
    for (let i = 0; i < 40; i += 1) {
      await frame()
    }

    expect(seen).not.toHaveBeenCalled()
  })

  it('reports nothing for a marker that is not intersecting', () => {
    const io = installIntersectionObserver()
    const { root, marker } = mounted()
    const seen = vi.fn()
    observeNewestSeen(marker, root, () => true, seen)

    io.fire(false)

    expect(seen).not.toHaveBeenCalled()
  })

  it('cancels a pending wait when disconnected', async () => {
    const io = installIntersectionObserver()
    const { root, marker } = mounted()
    let revealed = false
    const seen = vi.fn()
    const stop = observeNewestSeen(marker, root, () => revealed, seen)

    io.fire(true)
    stop()
    revealed = true
    await frame()
    await frame()

    expect(io.disconnect).toHaveBeenCalled()
    expect(seen).not.toHaveBeenCalled()
  })
})

describe('anchorCaughtUp', () => {
  const seen = { scope: 'a\u0000r', anchor: '$tap', eventId: '$newest' }
  const view = {
    seen,
    scope: 'a\u0000r',
    anchor: '$tap',
    atEnd: true,
    newestRenderedId: '$newest',
  }

  it('releases only for the current scope, anchor and newest row at the live end', () => {
    expect(anchorCaughtUp(view)).toBe(true)
    expect(anchorCaughtUp({ ...view, atEnd: false })).toBe(false)
    expect(anchorCaughtUp({ ...view, newestRenderedId: '$later' })).toBe(false)
    expect(anchorCaughtUp({ ...view, anchor: '$other-hit' })).toBe(false)
    expect(anchorCaughtUp({ ...view, scope: 'a\u0000other' })).toBe(false)
    expect(anchorCaughtUp({ ...view, seen: null })).toBe(false)
    expect(anchorCaughtUp({ ...view, anchor: null })).toBe(false)
  })
})
