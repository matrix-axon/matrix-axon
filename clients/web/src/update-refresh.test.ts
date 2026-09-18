import { afterEach, describe, expect, it, vi } from 'vitest'
import type { ReloadEnvironment } from './reload'
import {
  createUpdateChecker,
  type UpdateChecker,
  type VersionManifest,
} from './stores/update-check'
import { autoRefreshApplies, AWAY_MS, startAutoRefresh } from './update-refresh'
import { memoryStorage } from './test/memory-storage'

const NEWER: VersionManifest = {
  release: '0.1.0',
  version: 'build-b',
  builtAt: '2026-08-02T14:03:11.204Z',
}

/**
 * A stand-in document whose visibility a test drives directly. jsdom's
 * `document.hidden` is derived from `visibilityState` and not writable in a way
 * that survives a listener dispatch, so the seam is cleaner than the global.
 */
function fakeDoc() {
  const listeners = new Set<() => void>()
  return {
    hidden: false,
    addEventListener: (_type: string, fn: EventListener) => {
      listeners.add(fn as () => void)
    },
    removeEventListener: (_type: string, fn: EventListener) => {
      listeners.delete(fn as () => void)
    },
    /** Flip visibility and fire `visibilitychange`, as a browser would. */
    setHidden(hidden: boolean) {
      this.hidden = hidden
      for (const fn of listeners) fn()
    },
    listenerCount: () => listeners.size,
  }
}

function fakeWin() {
  const listeners = new Set<() => void>()
  return {
    addEventListener: (_type: string, fn: EventListener) => {
      listeners.add(fn as () => void)
    },
    removeEventListener: (_type: string, fn: EventListener) => {
      listeners.delete(fn as () => void)
    },
    goOnline() {
      for (const fn of listeners) fn()
    },
    listenerCount: () => listeners.size,
  }
}

interface HarnessOptions {
  manifest?: VersionManifest | null
  hasUnsentWork?: boolean
  currentVersion?: string
  flush?: () => Promise<void>
}

function harness(options: HarnessOptions = {}) {
  const doc = fakeDoc()
  const win = fakeWin()
  let reloads = 0
  let clock = 1_000_000
  const env: ReloadEnvironment = {
    storage: memoryStorage(),
    reload: () => {
      reloads += 1
    },
  }
  const updates: UpdateChecker = createUpdateChecker({
    currentVersion: options.currentVersion ?? 'build-a',
    fetchManifest: () =>
      Promise.resolve(
        options.manifest === undefined ? NEWER : options.manifest,
      ),
  })
  const flushed: number[] = []
  const dispose = startAutoRefresh({
    updates,
    currentVersion: options.currentVersion ?? 'build-a',
    flush:
      options.flush ??
      (() => {
        flushed.push(1)
        return Promise.resolve()
      }),
    hasUnsentWork: () => options.hasUnsentWork ?? false,
    doc: doc as unknown as Document,
    win: win as unknown as Window,
    env,
    now: () => clock,
  })
  return {
    doc,
    win,
    updates,
    dispose,
    reloads: () => reloads,
    flushes: () => flushed.length,
    advance: (ms: number) => {
      clock += ms
    },
  }
}

/** Let the checker's promise chain and the reload decision after it settle. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 0))

afterEach(() => {
  vi.restoreAllMocks()
})

describe('startAutoRefresh', () => {
  it('checks once at startup', async () => {
    const h = harness()
    await settle()
    expect(h.updates.available.value).toBe(true)
    h.dispose()
  })

  // The case the whole feature exists for: an installed PWA resumed from the
  // background, which today has to be force-killed to come back working.
  it('reloads on returning after a long absence', async () => {
    const h = harness()
    await settle()
    expect(h.reloads()).toBe(0) // visible and in use — no reload yet

    h.doc.setHidden(true)
    h.advance(AWAY_MS + 1)
    h.doc.setHidden(false)
    await settle()

    expect(h.reloads()).toBe(1)
    expect(h.flushes()).toBe(1) // drafts pushed before the page went away
    h.dispose()
  })

  // A short hide is a tab switch, a notification shade, an autofill sheet.
  // Reloading through one is indistinguishable from the app crashing.
  it('does not reload on returning from a brief hide', async () => {
    const h = harness()
    await settle()

    h.doc.setHidden(true)
    h.advance(5000)
    h.doc.setHidden(false)
    await settle()

    expect(h.reloads()).toBe(0)
    expect(h.updates.available.value).toBe(true) // the banner will offer it
    h.dispose()
  })

  it('reloads a tab that goes stale while hidden', async () => {
    // Nothing available at startup; the update appears while the tab is hidden.
    let manifest: VersionManifest | null = null
    const doc = fakeDoc()
    const win = fakeWin()
    let reloads = 0
    const updates = createUpdateChecker({
      currentVersion: 'build-a',
      fetchManifest: () => Promise.resolve(manifest),
    })
    const dispose = startAutoRefresh({
      updates,
      currentVersion: 'build-a',
      flush: () => Promise.resolve(),
      hasUnsentWork: () => false,
      doc: doc as unknown as Document,
      win: win as unknown as Window,
      env: {
        storage: memoryStorage(),
        reload: () => {
          reloads += 1
        },
      },
      now: () => 0,
    })
    await settle()
    expect(reloads).toBe(0)

    doc.hidden = true
    manifest = NEWER
    await updates.check()
    await settle()

    expect(reloads).toBe(1)
    dispose()
  })

  // An unsent echo lives only in memory: reloading would take the user's
  // message, or the File behind an in-flight upload, with it.
  it('never reloads while there is unsent work', async () => {
    const h = harness({ hasUnsentWork: true })
    await settle()

    h.doc.setHidden(true)
    h.advance(AWAY_MS * 10)
    h.doc.setHidden(false)
    await settle()

    expect(h.reloads()).toBe(0)
    expect(h.updates.available.value).toBe(true)
    h.dispose()
  })

  it('does not reload when the origin serves the build we are running', async () => {
    const h = harness({ manifest: { ...NEWER, version: 'build-a' } })
    await settle()

    h.doc.setHidden(true)
    h.advance(AWAY_MS + 1)
    h.doc.setHidden(false)
    await settle()

    expect(h.reloads()).toBe(0)
    h.dispose()
  })

  it('reloads at most once even if the trigger fires repeatedly', async () => {
    const h = harness()
    await settle()

    for (let i = 0; i < 3; i += 1) {
      h.doc.setHidden(true)
      h.advance(AWAY_MS + 1)
      h.doc.setHidden(false)
      await settle()
    }

    expect(h.reloads()).toBe(1)
    h.dispose()
  })

  // The half-finished deploy: the origin names a build it never serves, so the
  // page comes back exactly as it left. Reloading again would never terminate.
  it('reloads once against a manifest it can never satisfy, then stops', async () => {
    const storage = memoryStorage()
    const run = async () => {
      const doc = fakeDoc()
      const win = fakeWin()
      let reloads = 0
      let clock = 0
      const updates = createUpdateChecker({
        currentVersion: 'build-a',
        fetchManifest: () => Promise.resolve(NEWER),
      })
      const dispose = startAutoRefresh({
        updates,
        currentVersion: 'build-a',
        flush: () => Promise.resolve(),
        hasUnsentWork: () => false,
        doc: doc as unknown as Document,
        win: win as unknown as Window,
        // One storage across both runs is what makes this two boots of the
        // same tab rather than two tabs.
        env: {
          storage,
          reload: () => {
            reloads += 1
          },
        },
        now: () => clock,
      })
      await settle()
      doc.setHidden(true)
      clock += AWAY_MS + 1
      doc.setHidden(false)
      await settle()
      dispose()
      return reloads
    }

    expect(await run()).toBe(1) // first boot: reloads
    // Second boot lands on build-a again — the reload changed nothing, and the
    // guard (never cleared, because `from` still matches) refuses a repeat.
    expect(await run()).toBe(0)
  })

  // A flush that never resolves must not be what stops the reload — the page
  // we are trying to fix is already broken.
  it('reloads even when the draft flush hangs', async () => {
    vi.useFakeTimers()
    try {
      const h = harness({ flush: () => new Promise<void>(() => {}) })
      await vi.advanceTimersByTimeAsync(0)

      h.doc.setHidden(true)
      h.advance(AWAY_MS + 1)
      h.doc.setHidden(false)
      await vi.advanceTimersByTimeAsync(5000)

      expect(h.reloads()).toBe(1)
      h.dispose()
    } finally {
      vi.useRealTimers()
    }
  })

  it('checks when the network comes back', async () => {
    const h = harness({ manifest: null })
    await settle()
    expect(h.updates.status.value).toBe('error')

    h.advance(60_000) // past the visibility/online throttle
    h.win.goOnline()
    await settle()
    expect(h.updates.status.value).toBe('error')
    h.dispose()
  })

  it('removes its listeners on dispose', () => {
    const h = harness()
    expect(h.doc.listenerCount()).toBe(1)
    expect(h.win.listenerCount()).toBe(1)
    h.dispose()
    expect(h.doc.listenerCount()).toBe(0)
    expect(h.win.listenerCount()).toBe(0)
  })
})

describe('autoRefreshApplies', () => {
  it('runs for a deployed browser build, which is what it was designed for', () => {
    expect(autoRefreshApplies({ isDev: false, updatesFromOrigin: true })).toBe(
      true,
    )
  })

  it('stays out of the way under vite dev, where HMR owns reloading', () => {
    expect(autoRefreshApplies({ isDev: true, updatesFromOrigin: true })).toBe(
      false,
    )
  })

  it('does not run in a packaged build, whose bundle is its own binary', () => {
    // Otherwise: a timer and a request every 15 minutes to compare the shipped
    // manifest against the build that shipped it, plus a hidden-tab auto-reload
    // that would restart the app to fetch what it already has.
    expect(autoRefreshApplies({ isDev: false, updatesFromOrigin: false })).toBe(
      false,
    )
  })
})
