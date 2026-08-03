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
