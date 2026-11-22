import { describe, expect, it } from 'vitest'
import { createRoomStoreCache } from './room-store-cache'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const OTHER_ACCOUNT = '6b53f7f0-0000-4000-8000-000000000002'

function harness(limit: number) {
  const released: string[] = []
  let built = 0
  const cache = createRoomStoreCache(
    (accountId, roomId) => {
      built += 1
      return { key: `${accountId}/${roomId}` }
    },
    { limit, release: (store) => released.push(store.key) },
  )
  return { cache, released, built: () => built }
}

describe('createRoomStoreCache', () => {
  it('hands a room its warm store back instead of building another', () => {
    const { cache, built } = harness(3)
    const first = cache.acquire(ACCOUNT, '!a:hs')
    expect(cache.acquire(ACCOUNT, '!a:hs') === first).toBe(true)
    expect(built()).toBe(1)
    // Keyed by account too: the same room id on another account is another store.
    expect(cache.acquire(OTHER_ACCOUNT, '!a:hs') === first).toBe(false)
  })

  it('evicts the least recently acquired room past the cap, releasing it', () => {
    const { cache, released } = harness(2)
    cache.acquire(ACCOUNT, '!a:hs')
    cache.acquire(ACCOUNT, '!b:hs')
    // Re-acquiring `a` makes `b` the oldest.
    cache.acquire(ACCOUNT, '!a:hs')
    cache.acquire(ACCOUNT, '!c:hs')

    expect(cache.size).toBe(2)
    expect(released).toEqual([`${ACCOUNT}/!b:hs`])
  })

  it("drops one account's stores, releasing each, and keeps the rest", () => {
    const { cache, released, built } = harness(5)
    cache.acquire(ACCOUNT, '!a:hs')
    const kept = cache.acquire(OTHER_ACCOUNT, '!a:hs')
    cache.acquire(ACCOUNT, '!b:hs')

    cache.forgetAccount(ACCOUNT)

    expect(released).toEqual([`${ACCOUNT}/!a:hs`, `${ACCOUNT}/!b:hs`])
    expect(cache.size).toBe(1)
    expect(cache.acquire(OTHER_ACCOUNT, '!a:hs') === kept).toBe(true)
    expect(built()).toBe(3)
  })

  it('releases every store on clear', () => {
    const { cache, released } = harness(5)
    cache.acquire(ACCOUNT, '!a:hs')
    cache.acquire(ACCOUNT, '!b:hs')

    cache.clear()
    cache.clear()

    expect(cache.size).toBe(0)
    expect(released.sort()).toEqual([`${ACCOUNT}/!a:hs`, `${ACCOUNT}/!b:hs`])
  })
})
