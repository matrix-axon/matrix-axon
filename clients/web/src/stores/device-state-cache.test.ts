import { describe, expect, it } from 'vitest'
import { createMemoryCacheStore } from './cache-store'
import { createDeviceStateCache } from './device-state-cache'

const ACCT = '6b53f7f0-0000-4000-8000-000000000001'
const scope = (cursor: string) => ({
  cursor,
  entries: new Map<string, unknown>([
    ['!a:hs', { event_id: '$a', origin_ts: 1 }],
  ]),
})

/** Let the fire-and-forget write's async chain run to the end. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 0))

function setup() {
  const cache = createMemoryCacheStore()
  let reader: string | null = 'reader-1'
  let enabled = true
  const deviceStateCache = createDeviceStateCache({
    cache,
    namespace: () => Promise.resolve(reader),
    enabled: () => enabled,
  })
  return {
    cache,
    deviceStateCache,
    setReader: (next: string | null) => {
      reader = next
    },
    setEnabled: (next: boolean) => {
      enabled = next
    },
  }
}

describe('createDeviceStateCache (ADR 0131)', () => {
  it('round-trips a marker scope with its cursor', async () => {
    const { deviceStateCache } = setup()
    deviceStateCache.write(ACCT, 'read_markers', scope('100'))
    await settle()
    const read = await deviceStateCache.read(ACCT, 'read_markers')
    expect(read?.cursor).toBe('100')
    expect(read?.entries.get('!a:hs')).toEqual({ event_id: '$a', origin_ts: 1 })
    expect(await deviceStateCache.read(ACCT, 'thread_read_markers')).toBe(
      undefined,
    )
  })

  it('never writes drafts to disk: they are message text (ADR 0085)', async () => {
    const { cache, deviceStateCache } = setup()
    deviceStateCache.write(ACCT, 'drafts', scope('100'))
    await settle()
    expect(await cache.keys('meta')).toEqual([])
    expect(await deviceStateCache.read(ACCT, 'drafts')).toBe(undefined)
  })

  it('reads and writes nothing while the room-list cache is off', async () => {
    const { cache, deviceStateCache, setEnabled } = setup()
    deviceStateCache.write(ACCT, 'read_markers', scope('100'))
    await settle()
    setEnabled(false)
    expect(await deviceStateCache.read(ACCT, 'read_markers')).toBe(undefined)
    deviceStateCache.write(ACCT, 'read_markers', scope('200'))
    await settle()
    setEnabled(true)
    expect((await deviceStateCache.read(ACCT, 'read_markers'))?.cursor).toBe(
      '100',
    )
    expect(await cache.keys('meta')).toHaveLength(1)
  })

  it('is keyed by reader, and drops a previous reader on write', async () => {
    const { cache, deviceStateCache, setReader } = setup()
    await cache.write('meta', 'telemetry.v1', { kept: true })
    deviceStateCache.write(ACCT, 'read_markers', scope('100'))
    await settle()

    setReader('reader-2')
    expect(await deviceStateCache.read(ACCT, 'read_markers')).toBe(undefined)
    deviceStateCache.write(ACCT, 'thread_read_markers', scope('200'))
    await settle()
    const keys = await cache.keys('meta')
    expect(keys).toContain('telemetry.v1')
    expect(keys.filter((key) => key.includes('reader-1'))).toEqual([])
    expect(keys.filter((key) => key.includes('reader-2'))).toHaveLength(1)
  })

  it('writes nothing for a signed-out reader', async () => {
    const { cache, deviceStateCache, setReader } = setup()
    setReader(null)
    deviceStateCache.write(ACCT, 'read_markers', scope('100'))
    await settle()
    expect(await cache.keys('meta')).toEqual([])
  })

  it('loses to a wipe that starts before the write lands', async () => {
    const { cache, deviceStateCache } = setup()
    deviceStateCache.write(ACCT, 'read_markers', scope('100'))
    void cache.clear()
    await settle()
    expect(await cache.keys('meta')).toEqual([])
  })

  it('ignores a record of the wrong shape', async () => {
    const { cache, deviceStateCache } = setup()
    deviceStateCache.write(ACCT, 'read_markers', scope('100'))
    await settle()
    const [key] = await cache.keys('meta')
    await cache.write('meta', key, { version: 1, cursor: 5, entries: {} })
    expect(await deviceStateCache.read(ACCT, 'read_markers')).toBe(undefined)
    await cache.write('meta', key, { version: 99, cursor: '1', entries: {} })
    expect(await deviceStateCache.read(ACCT, 'read_markers')).toBe(undefined)
  })
})
