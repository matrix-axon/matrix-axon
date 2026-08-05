import { describe, expect, it, vi } from 'vitest'
import {
  createUpdateChecker,
  fetchVersionManifest,
  parseVersionManifest,
  type VersionManifest,
} from './update-check'

const MANIFEST: VersionManifest = {
  release: '0.1.0',
  version: 'build-b',
  builtAt: '2026-08-02T14:03:11.204Z',
}

function jsonResponse(body: unknown, contentType = 'application/json') {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { 'content-type': contentType },
  })
}

describe('parseVersionManifest', () => {
  it('accepts a well-formed manifest', () => {
    expect(parseVersionManifest(MANIFEST)).toEqual(MANIFEST)
  })

  it('keeps a real hash that happens to be dirty', () => {
    expect(
      parseVersionManifest({ version: 'ab12cd34ef56-dirty' })?.version,
    ).toBe('ab12cd34ef56-dirty')
  })

  it('defaults the display-only fields when they are missing', () => {
    expect(parseVersionManifest({ version: 'abc' })).toEqual({
      release: '',
      version: 'abc',
      builtAt: '',
    })
  })

  it.each([
    ['a non-object', 'nope'],
    ['null', null],
    ['no version', { release: '0.1.0' }],
    ['a non-string version', { version: 7 }],
    ['an empty version', { version: '' }],
    // Every build made without git reports `unknown`, so it distinguishes
    // nothing — treating it as a version would make two anonymous builds look
    // identical and an anonymous build look newer than a hashed one.
    ['the `unknown` placeholder', { version: 'unknown' }],
    // `-dirty` is appended to whatever the hash lookup produced, so a gitless
    // build in a dirty tree says this. Just as anonymous, and it was slipping
    // through as a real id.
    ['the `unknown-dirty` placeholder', { version: 'unknown-dirty' }],
  ])('rejects %s', (_label, value) => {
    expect(parseVersionManifest(value)).toBeNull()
  })
})

describe('fetchVersionManifest', () => {
  it('reads a JSON manifest and asks for no cached copy', async () => {
    const fetchImpl = vi.fn().mockResolvedValue(jsonResponse(MANIFEST))
    await expect(
      fetchVersionManifest('/version.json', fetchImpl),
    ).resolves.toEqual(MANIFEST)
    expect(fetchImpl).toHaveBeenCalledWith('/version.json', {
      cache: 'no-store',
      headers: { accept: 'application/json' },
      signal: expect.any(AbortSignal),
    })
  })

  // The failure this whole function is shaped around: an SPA server with a
  // history fallback answers a *missing* /version.json with index.html and a
  // 200, so "not deployed yet" would otherwise read as "a different build".
  it('rejects an HTML body served with a 200 by the SPA fallback', async () => {
    const fetchImpl = vi.fn().mockResolvedValue(
      new Response('<!doctype html><html></html>', {
        status: 200,
        headers: { 'content-type': 'text/html' },
      }),
    )
    await expect(
      fetchVersionManifest('/version.json', fetchImpl),
    ).resolves.toBeNull()
  })

  it('rejects a non-ok response', async () => {
    const fetchImpl = vi
      .fn()
      .mockResolvedValue(new Response('', { status: 404 }))
    await expect(
      fetchVersionManifest('/version.json', fetchImpl),
    ).resolves.toBeNull()
  })

  it('swallows a network error', async () => {
    const fetchImpl = vi.fn().mockRejectedValue(new TypeError('offline'))
    await expect(
      fetchVersionManifest('/version.json', fetchImpl),
    ).resolves.toBeNull()
  })

  /**
   * A hung read is worse than a failed one: it never resolves, so the caller's
   * de-duplication slot never clears and update detection stops for the life of
   * the tab. The bound has to cover the body, not just the connection — a
   * socket that opens and then stalls mid-body hangs `response.json()` exactly
   * as effectively as one that never answers.
   */
  it('gives up on a connection that never answers', async () => {
    vi.useFakeTimers()
    try {
      const fetchImpl = vi.fn(
        (_path: RequestInfo | URL, init?: RequestInit) =>
          new Promise<Response>((_resolve, reject) => {
            init?.signal?.addEventListener('abort', () =>
              reject(new DOMException('aborted', 'AbortError')),
            )
          }),
      )
      const read = fetchVersionManifest('/version.json', fetchImpl, 5000)
      let settled = false
      void read.then(() => {
        settled = true
      })

      await vi.advanceTimersByTimeAsync(4999)
      expect(settled).toBe(false)

      await vi.advanceTimersByTimeAsync(2)
      await expect(read).resolves.toBeNull()
    } finally {
      vi.useRealTimers()
    }
  })

  it('gives up on a body that stalls after the headers', async () => {
    vi.useFakeTimers()
    try {
      const fetchImpl = vi.fn(
        (_path: RequestInfo | URL, init?: RequestInit) =>
          Promise.resolve({
            ok: true,
            headers: new Headers({ 'content-type': 'application/json' }),
            json: () =>
              new Promise((_resolve, reject) => {
                init?.signal?.addEventListener('abort', () =>
                  reject(new DOMException('aborted', 'AbortError')),
                )
              }),
          }) as unknown as Promise<Response>,
      )
      const read = fetchVersionManifest('/version.json', fetchImpl, 5000)
      await vi.advanceTimersByTimeAsync(5001)
      await expect(read).resolves.toBeNull()
    } finally {
      vi.useRealTimers()
    }
  })

  it('swallows a JSON content type carrying a broken body', async () => {
    const fetchImpl = vi.fn().mockResolvedValue(
      new Response('{ not json', {
        status: 200,
        headers: { 'content-type': 'application/json' },
      }),
    )
    await expect(
      fetchVersionManifest('/version.json', fetchImpl),
    ).resolves.toBeNull()
  })
})

describe('createUpdateChecker', () => {
  it('reports an update when the origin serves a different build', async () => {
    const checker = createUpdateChecker({
      currentVersion: 'build-a',
      fetchManifest: () => Promise.resolve(MANIFEST),
    })
    await checker.check()
    expect(checker.available.value).toBe(true)
    expect(checker.status.value).toBe('available')
    expect(checker.latest.value).toEqual(MANIFEST)
  })

  it('stays quiet when the origin serves the build we are running', async () => {
    const checker = createUpdateChecker({
      currentVersion: 'build-b',
      fetchManifest: () => Promise.resolve(MANIFEST),
    })
    await checker.check()
    expect(checker.available.value).toBe(false)
    expect(checker.status.value).toBe('current')
  })

  it('stays quiet when the check learns nothing', async () => {
    const checker = createUpdateChecker({
      currentVersion: 'build-a',
      fetchManifest: () => Promise.resolve(null),
    })
    await checker.check()
    expect(checker.available.value).toBe(false)
    expect(checker.status.value).toBe('error')
  })

  // A deploy restarts the server, so a poll can easily straddle the restart and
  // read the old manifest again. Retracting the banner there would flicker it,
  // and the new build has not stopped existing.
  it('latches: a later check reading our own version does not retract it', async () => {
    let manifest: VersionManifest | null = MANIFEST
    const checker = createUpdateChecker({
      currentVersion: 'build-a',
      fetchManifest: () => Promise.resolve(manifest),
    })
    await checker.check()
    expect(checker.available.value).toBe(true)

    manifest = { ...MANIFEST, version: 'build-a' }
    await checker.check()
    expect(checker.available.value).toBe(true)
    expect(checker.status.value).toBe('available')
  })

  it('collapses overlapping checks into one request', async () => {
    const fetchManifest = vi.fn().mockResolvedValue(MANIFEST)
    const checker = createUpdateChecker({
      currentVersion: 'build-a',
      fetchManifest,
    })
    await Promise.all([checker.check(), checker.check(), checker.check()])
    expect(fetchManifest).toHaveBeenCalledTimes(1)

    // …but a later check is a new request, not a cached answer.
    await checker.check()
    expect(fetchManifest).toHaveBeenCalledTimes(2)
  })

  /**
   * The slot that de-duplicates every trigger must always clear. A
   * `fetchManifest` that never settles would otherwise pin it and turn every
   * later check into the same stuck promise — detection off for the tab's life.
   */
  it('does not wedge when fetchManifest never settles', async () => {
    vi.useFakeTimers()
    try {
      let calls = 0
      const checker = createUpdateChecker({
        currentVersion: 'build-a',
        fetchManifest: () => {
          calls += 1
          return calls === 1
            ? new Promise<VersionManifest | null>(() => {})
            : Promise.resolve(MANIFEST)
        },
      })

      const stuck = checker.check()
      await vi.advanceTimersByTimeAsync(60_000)
      await expect(stuck).resolves.toBeUndefined()

      // The slot is free, so a later check runs for real rather than handing
      // back the abandoned one.
      await checker.check()
      expect(calls).toBe(2)
      expect(checker.available.value).toBe(true)
    } finally {
      vi.useRealTimers()
    }
  })

  /**
   * The watchdog frees the de-duplication slot, but nothing can cancel the run
   * behind it. A run abandoned that way must not surface its answer later and
   * overwrite one a newer check has already produced. Raised in review of #114:
   * not reachable via the shipped `fetchVersionManifest`, which aborts itself,
   * but `fetchManifest` is an injection point.
   */
  it('ignores a late answer from a run the watchdog abandoned', async () => {
    vi.useFakeTimers()
    try {
      let releaseStale: ((m: VersionManifest | null) => void) | undefined
      let calls = 0
      const checker = createUpdateChecker({
        currentVersion: 'build-a',
        fetchManifest: () => {
          calls += 1
          return calls === 1
            ? new Promise<VersionManifest | null>((resolve) => {
                releaseStale = resolve
              })
            : Promise.resolve({ ...MANIFEST, version: 'build-current' })
        },
      })

      const abandoned = checker.check()
      await vi.advanceTimersByTimeAsync(60_000)
      await abandoned // watchdog freed the slot

      // A newer check lands a real answer.
      await checker.check()
      expect(checker.latest.value?.version).toBe('build-current')

      // Now the abandoned run finally answers, with something different.
      releaseStale!({ ...MANIFEST, version: 'stale-answer' })
      await vi.advanceTimersByTimeAsync(0)

      expect(checker.latest.value?.version).toBe('build-current')
      expect(checker.status.value).toBe('available')
    } finally {
      vi.useRealTimers()
    }
  })

  it('ignores a late error from an abandoned run', async () => {
    vi.useFakeTimers()
    try {
      let releaseStale: ((m: VersionManifest | null) => void) | undefined
      let calls = 0
      const checker = createUpdateChecker({
        currentVersion: 'build-a',
        fetchManifest: () => {
          calls += 1
          return calls === 1
            ? new Promise<VersionManifest | null>((resolve) => {
                releaseStale = resolve
              })
            : Promise.resolve(MANIFEST)
        },
      })

      const abandoned = checker.check()
      await vi.advanceTimersByTimeAsync(60_000)
      await abandoned
      await checker.check()
      expect(checker.available.value).toBe(true)

      // `null` is "learned nothing" — it must not downgrade `status` to error
      // after a newer check has reported an available build.
      releaseStale!(null)
      await vi.advanceTimersByTimeAsync(0)
      expect(checker.status.value).toBe('available')
      expect(checker.available.value).toBe(true)
    } finally {
      vi.useRealTimers()
    }
  })

  it('polls on an interval once started, and stops on stop()', async () => {
    vi.useFakeTimers()
    try {
      const fetchManifest = vi.fn().mockResolvedValue(null)
      const checker = createUpdateChecker({
        currentVersion: 'build-a',
        fetchManifest,
        intervalMs: 1000,
      })
      checker.start()
      checker.start() // idempotent
      await vi.advanceTimersByTimeAsync(3000)
      expect(fetchManifest).toHaveBeenCalledTimes(3)

      checker.stop()
      await vi.advanceTimersByTimeAsync(3000)
      expect(fetchManifest).toHaveBeenCalledTimes(3)
    } finally {
      vi.useRealTimers()
    }
  })

  it('skips a scheduled poll while the document is hidden', async () => {
    vi.useFakeTimers()
    try {
      const fetchManifest = vi.fn().mockResolvedValue(null)
      let visible = false
      const checker = createUpdateChecker({
        currentVersion: 'build-a',
        fetchManifest,
        intervalMs: 1000,
        isVisible: () => visible,
      })
      checker.start()
      await vi.advanceTimersByTimeAsync(3000)
      expect(fetchManifest).not.toHaveBeenCalled()

      visible = true
      await vi.advanceTimersByTimeAsync(1000)
      expect(fetchManifest).toHaveBeenCalledTimes(1)
      checker.stop()
    } finally {
      vi.useRealTimers()
    }
  })
})
