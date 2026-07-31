import { describe, expect, it } from 'vitest'
import {
  cacheNamespace,
  createIndexedDbCacheStore,
  createMemoryCacheStore,
} from './cache-store'

describe('memory cache store', () => {
  it('round-trips a value under an area and key', async () => {
    const cache = createMemoryCacheStore()
    await cache.write('rooms', 'k', { rooms: [1, 2] })
    expect(await cache.read('rooms', 'k')).toEqual({ rooms: [1, 2] })
  })

  it('keeps areas and keys apart', async () => {
    const cache = createMemoryCacheStore()
    await cache.write('rooms', 'k', 'rooms-value')
    await cache.write('timelines', 'k', 'timeline-value')
    expect(await cache.read('rooms', 'k')).toBe('rooms-value')
    expect(await cache.read('timelines', 'k')).toBe('timeline-value')
    expect(await cache.read('rooms', 'other')).toBeUndefined()
  })

  it('clones on the way in, as IndexedDB does', async () => {
    const cache = createMemoryCacheStore()
    const value = { rooms: ['a'] }
    await cache.write('rooms', 'k', value)
    value.rooms.push('b')
    expect(await cache.read<typeof value>('rooms', 'k')).toEqual({
      rooms: ['a'],
    })
  })

  it('drops one key and clears every area', async () => {
    const cache = createMemoryCacheStore()
    await cache.write('rooms', 'a', 1)
    await cache.write('rooms', 'b', 2)
    await cache.write('meta', 'c', 3)
    await cache.drop('rooms', 'a')
    expect(await cache.keys('rooms')).toEqual(['b'])
    await cache.clear()
    expect(await cache.keys('rooms')).toEqual([])
    expect(await cache.read('meta', 'c')).toBeUndefined()
  })
})

describe('indexeddb cache store', () => {
  // jsdom has no IndexedDB, which is the *point* of this test: the adapter has
  // to degrade to "no cache" rather than throwing into whatever store called
  // it. Every path resolves; none rejects.
  it('degrades silently when the platform has no IndexedDB', async () => {
    const cache = createIndexedDbCacheStore(undefined)
    await expect(cache.write('rooms', 'k', 1)).resolves.toBeUndefined()
    await expect(cache.read('rooms', 'k')).resolves.toBeUndefined()
    await expect(cache.keys('rooms')).resolves.toEqual([])
    await expect(cache.drop('rooms', 'k')).resolves.toBeUndefined()
    await expect(cache.clear()).resolves.toBeUndefined()
  })

  it('degrades silently when opening the database throws', async () => {
    const factory = {
      open: () => {
        throw new Error('storage denied')
      },
    } as unknown as IDBFactory
    const cache = createIndexedDbCacheStore(factory)
    await expect(cache.read('rooms', 'k')).resolves.toBeUndefined()
  })

  it('degrades silently when the open request errors', async () => {
    const factory = {
      open: () => {
        const request = {
          onsuccess: null as unknown,
          onerror: null as (() => void) | null,
          onupgradeneeded: null as unknown,
          onblocked: null as unknown,
        }
        queueMicrotask(() => request.onerror?.())
        return request
      },
    } as unknown as IDBFactory
    const cache = createIndexedDbCacheStore(factory)
    await expect(cache.read('rooms', 'k')).resolves.toBeUndefined()
  })
})

describe('cacheNamespace', () => {
  it('is null without a token, so nothing is cached', async () => {
    expect(await cacheNamespace('http://axon.test', null)).toBeNull()
    expect(await cacheNamespace('http://axon.test', '')).toBeNull()
  })

  it('separates readers: a different token is a different namespace', async () => {
    const one = await cacheNamespace('http://axon.test', 'token-one')
    const two = await cacheNamespace('http://axon.test', 'token-two')
    expect(one).not.toBe(two)
  })

  it('separates servers: the same token on another base is another namespace', async () => {
    const one = await cacheNamespace('http://axon.test', 'tok')
    const two = await cacheNamespace('http://other.test', 'tok')
    expect(one).not.toBe(two)
  })

  it('is stable for the same reader', async () => {
    expect(await cacheNamespace('http://axon.test', 'tok')).toBe(
      await cacheNamespace('http://axon.test', 'tok'),
    )
  })

  it('never contains the token itself', async () => {
    const namespace = await cacheNamespace('http://axon.test', 'super-secret')
    expect(namespace).not.toContain('super-secret')
  })
})
