import { describe, expect, it, vi } from 'vitest'
import {
  BUDGET_WINDOW_MS,
  CHUNK_TARGET,
  initReloadGuard,
  MAX_ATTEMPTS,
  reloadNow,
  reloadOnce,
  type ReloadEnvironment,
} from './reload'
import { memoryStorage } from './test/memory-storage'

function env(
  storage: Storage | null = memoryStorage(),
  clock: { now: number } = { now: 1_000_000 },
): ReloadEnvironment & { reloads: () => number } {
  let reloads = 0
  return {
    storage,
    reload: () => {
      reloads += 1
    },
    now: () => clock.now,
    reloads: () => reloads,
  }
}

describe('reloadOnce', () => {
  it('reloads the first time and records the attempt', () => {
    const e = env()
    expect(reloadOnce('build-a', 'build-b', e)).toBe(true)
    expect(e.reloads()).toBe(1)
    expect(JSON.parse(e.storage!.getItem('axon:update-reload')!)).toMatchObject(
      {
        from: 'build-a',
        targets: ['build-b'],
        spent: 1,
      },
    )
  })

  // The loop this module exists to prevent: we reload, and come back running
  // the very build we left (a partial deploy, a stale manifest behind a cache).
  // Reloading again would achieve exactly as much, forever.
  it('refuses to repeat the same attempt from the same build', () => {
    const e = env()
    reloadOnce('build-a', 'build-b', e)
    expect(reloadOnce('build-a', 'build-b', e)).toBe(false)
    expect(e.reloads()).toBe(1)
  })

  // The reason the guard keys on the target and not just the departed build:
  // one bad manifest must not disable automatic refresh for the whole session.
  it('still allows a reload toward a genuinely different build', () => {
    const e = env()
    expect(reloadOnce('build-a', 'never-ships', e)).toBe(true)
    expect(reloadOnce('build-a', 'never-ships', e)).toBe(false)
    expect(reloadOnce('build-a', 'build-c', e)).toBe(true)
    expect(e.reloads()).toBe(2)
  })

  it('bounds a server that keeps naming new builds it does not serve', () => {
    const e = env()
    for (let i = 0; i < MAX_ATTEMPTS + 3; i += 1) {
      reloadOnce('build-a', `phantom-${i}`, e)
    }
    expect(e.reloads()).toBe(MAX_ATTEMPTS)
  })

  it('tracks a chunk failure separately from a version update', () => {
    const e = env()
    expect(reloadOnce('build-a', CHUNK_TARGET, e)).toBe(true)
    expect(reloadOnce('build-a', CHUNK_TARGET, e)).toBe(false)
    expect(reloadOnce('build-a', 'build-b', e)).toBe(true)
    expect(e.reloads()).toBe(2)
  })

  it('settles the pair once a boot lands on a new build', () => {
    const storage = memoryStorage()
    reloadOnce('build-a', 'build-b', env(storage))

    // Next boot, on build-b: the reload worked, so the *pair* record is spent
    // — but the budget it consumed is not refunded.
    initReloadGuard('build-b', env(storage))
    expect(JSON.parse(storage.getItem('axon:update-reload')!)).toMatchObject({
      from: 'build-b',
      targets: [],
      spent: 1,
    })

    const e = env(storage)
    expect(reloadOnce('build-b', 'build-c', e)).toBe(true)
    expect(e.reloads()).toBe(1)
  })

  it('keeps the record when a boot lands on the same build again', () => {
    const storage = memoryStorage()
    reloadOnce('build-a', 'never-ships', env(storage))

    initReloadGuard('build-a', env(storage))
    expect(JSON.parse(storage.getItem('axon:update-reload')!)).toMatchObject({
      from: 'build-a',
      targets: ['never-ships'],
    })

    const e = env(storage)
    expect(reloadOnce('build-a', 'never-ships', e)).toBe(false)
    expect(e.reloads()).toBe(0)
  })

  it('discards a record left by a different build', () => {
    const storage = memoryStorage()
    storage.setItem(
      'axon:update-reload',
      JSON.stringify({ from: 'some-older-build', targets: ['x', 'y', 'z'] }),
    )
    const e = env(storage)
    expect(reloadOnce('build-a', 'build-b', e)).toBe(true)
    expect(e.reloads()).toBe(1)
  })

  /**
   * The budget has to be cumulative across boots, not per build. An
   * inconsistent deployment can hand out a different build every time — A names
   * C, C names E, E names A — so no pair ever repeats and the per-pair rule
   * alone never fires. Clearing the record on a successful reload refilled the
   * budget on every boot, and the tab reloaded without end.
   */
  it('bounds a chain of distinct builds that never repeats a pair', () => {
    const storage = memoryStorage()
    const chain = ['build-b', 'build-c', 'build-d', 'build-e', 'build-f']
    let current = 'build-a'
    let reloads = 0

    for (const target of chain) {
      // Each boot: settle the previous pair, then try to move to a new build.
      initReloadGuard(current, env(storage))
      const e = env(storage)
      if (reloadOnce(current, target, e)) {
        reloads += e.reloads()
        current = target // the reload genuinely landed on the new bundle
      }
    }

    expect(reloads).toBe(MAX_ATTEMPTS)
  })

  it('refills the budget once the window has passed', () => {
    const storage = memoryStorage()
    const clock = { now: 1_000_000 }
    for (let i = 0; i < MAX_ATTEMPTS + 2; i += 1) {
      reloadOnce('build-a', `phantom-${i}`, env(storage, clock))
    }
    const spent = env(storage, clock)
    expect(reloadOnce('build-a', 'another', spent)).toBe(false)

    // A tab open for days must still pick up genuine deploys, so the cap bounds
    // the *rate* rather than the lifetime.
    clock.now += BUDGET_WINDOW_MS
    const later = env(storage, clock)
    expect(reloadOnce('build-a', 'after-the-window', later)).toBe(true)
    expect(later.reloads()).toBe(1)
  })

  it('treats unreadable guard state as absent', () => {
    const storage = memoryStorage()
    storage.setItem('axon:update-reload', 'not json at all')
    const e = env(storage)
    expect(reloadOnce('build-a', 'build-b', e)).toBe(true)
    // …and rewrites it readable, so failing open cannot repeat.
    expect(JSON.parse(storage.getItem('axon:update-reload')!)).toMatchObject({
      from: 'build-a',
      targets: ['build-b'],
      spent: 1,
    })
  })

  // Failing closed: an automatic reload we cannot bound is the one thing worse
  // than not reloading, and the fallback (a banner) is not a bad outcome.
  it('refuses to reload when sessionStorage is unavailable', () => {
    const e = env(null)
    expect(reloadOnce('build-a', 'build-b', e)).toBe(false)
    expect(e.reloads()).toBe(0)
  })

  it('refuses to reload when the guard cannot be written', () => {
    const storage = memoryStorage()
    vi.spyOn(storage, 'setItem').mockImplementation(() => {
      throw new DOMException('QuotaExceededError')
    })
    const e = env(storage)
    expect(reloadOnce('build-a', 'build-b', e)).toBe(false)
    expect(e.reloads()).toBe(0)
  })
})

describe('reloadNow', () => {
  it('reloads unconditionally, even when the guard would refuse', () => {
    const e = env()
    reloadOnce('build-a', 'build-b', e)
    expect(reloadOnce('build-a', 'build-b', e)).toBe(false)
    reloadNow(e)
    reloadNow(e)
    expect(e.reloads()).toBe(3)
  })

  it('reloads with no storage at all', () => {
    const e = env(null)
    reloadNow(e)
    expect(e.reloads()).toBe(1)
  })
})
