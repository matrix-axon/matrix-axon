import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createApiClient } from '../api/client'
import { memoryStorage } from '../test/memory-storage'
import { cacheNamespace, createMemoryCacheStore } from './cache-store'
import { createRoomListCache } from './room-list-cache'
import type { RoomDto } from './room-list'
import { createRoomsStore } from './rooms'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'

const OPS = {
  account_id: ACCOUNT,
  account_user_id: '@me:example.org',
  room_id: '!ops:hs',
  name: 'Ops',
  last_activity_ts: 100,
  notification_count: 3,
  highlight_count: 1,
} as RoomDto

const LOUNGE = {
  account_id: ACCOUNT,
  account_user_id: '@me:example.org',
  room_id: '!lounge:hs',
  name: 'Lounge',
  last_activity_ts: 200,
  notification_count: 0,
  highlight_count: 0,
} as RoomDto

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const api = () =>
  createApiClient(
    {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )

interface HarnessOptions {
  token?: string | null
  enabled?: () => boolean
  cache?: ReturnType<typeof createMemoryCacheStore>
}

function harness(options: HarnessOptions = {}) {
  const cache = options.cache ?? createMemoryCacheStore()
  const roomList = createRoomListCache({
    cache,
    namespace: () =>
      cacheNamespace(
        BASE_URL,
        options.token === undefined ? 'tok-test' : options.token,
      ),
    enabled: options.enabled ?? (() => true),
  })
  return { cache, roomList }
}

/**
 * Let the hydrate read and any fire-and-forget write settle. Microtask ticks
 * are not enough: the namespace is a `crypto.subtle` digest, which resolves off
 * the microtask queue, so every cache operation is at least one task deep.
 */
const settle = async () => {
  for (let i = 0; i < 3; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 0))
  }
}

/**
 * Wait for a restore to actually land, rather than for a fixed number of
 * ticks. `settle()` is fine for "let a fire-and-forget write finish", but a
 * test that goes on to assert *about* restored rows must not race the read:
 * three tasks is enough on an idle machine and not enough on a loaded one, so
 * a `settle()` there fails roughly one run in five under a full suite.
 */
const waitForRows = async (store: { rooms: { value: unknown[] } }) => {
  for (let i = 0; i < 200; i += 1) {
    if (store.rooms.value.length > 0) return
    await new Promise((resolve) => setTimeout(resolve, 5))
  }
  throw new Error('timed out waiting for the cache restore')
}

describe('room-list cache', () => {
  it('persists the rows a settled refresh returned', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [OPS, LOUNGE] }),
      ),
    )
    const { roomList } = harness()
    const store = createRoomsStore(api(), memoryStorage(), roomList)
    await store.refresh()
    await settle()
    const cached = await roomList.read()
    expect(cached?.map((room) => room.room_id)).toEqual([
      '!ops:hs',
      '!lounge:hs',
    ])
    expect(cached?.[0].notification_count).toBe(3)
  })

  it('paints cached rows before the network answers, marked stale', async () => {
    const { cache, roomList } = harness()
    roomList.write([OPS, LOUNGE])
    await settle()

    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => (release = resolve))
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, async () => {
        await held
        return HttpResponse.json({ data: [OPS, LOUNGE] })
      }),
    )
    const store = createRoomsStore(
      api(),
      memoryStorage(),
      harness({ cache }).roomList,
    )
    const refreshing = store.refresh()
    await waitForRows(store)

    // The whole point: rows are on screen while the request is still open.
    expect(store.loading.value).toBe(false)
    expect(store.stale.value).toBe(true)
    expect(store.rooms.value.map((room) => room.room_id)).toEqual([
      '!ops:hs',
      '!lounge:hs',
    ])
    // Unread badges ride along, so the restore is not silently unbadged.
    expect(store.unreadTotal.value).toBe(3)

    release?.()
    await refreshing
    expect(store.stale.value).toBe(false)
  })

  it('keeps cached rows when the refresh fails, still stale', async () => {
    const { cache, roomList } = harness()
    roomList.write([OPS])
    await settle()
    server.use(http.get(`${BASE_URL}/v1/rooms`, () => HttpResponse.error()))
    const store = createRoomsStore(
      api(),
      memoryStorage(),
      harness({ cache }).roomList,
    )
    await waitForRows(store)
    await store.refresh()
    expect(store.rooms.value.map((room) => room.room_id)).toEqual(['!ops:hs'])
    expect(store.stale.value).toBe(true)
    expect(store.error.value).not.toBeNull()
  })

  it('survives an error response with no body, without a TypeError', async () => {
    const { cache, roomList } = harness()
    roomList.write([OPS])
    await settle()
    // What a dev proxy returns when the API is unreachable, and what iOS
    // Safari reported as `undefined is not an object (evaluating 'data.data')`:
    // a non-2xx with an empty body parses into *neither* envelope, so a guard
    // on `error` alone falls through to `data.data`.
    // `Content-Length: 0` is load-bearing, not incidental: openapi-fetch
    // returns `{ error: undefined }` only for 204, HEAD, or exactly this
    // header (`src/index.js:238-245`). Without it the body parses to `""`,
    // the `error` guard catches it, and this test passes against the bug.
    server.use(
      http.get(
        `${BASE_URL}/v1/rooms`,
        () =>
          new HttpResponse(null, {
            status: 502,
            headers: { 'Content-Length': '0' },
          }),
      ),
    )
    const store = createRoomsStore(
      api(),
      memoryStorage(),
      harness({ cache }).roomList,
    )
    await waitForRows(store)
    await store.refresh()

    expect(store.rooms.value.map((room) => room.room_id)).toEqual(['!ops:hs'])
    expect(store.stale.value).toBe(true)
    // A usable message, not the text of a JavaScript TypeError.
    expect(store.error.value).toBe('unexpected server response')
  })

  it('starts caching once a token arrives mid-session', async () => {
    // Token-paste sign-in does not reload the document, so the graph outlives
    // the signed-out state it was built in. Resolving the namespace once at
    // startup meant this session wrote nothing at all, and the launch *after* a
    // sign-in was still cold — on a phone, a launch the user sees.
    const cache = createMemoryCacheStore()
    let token: string | null = null
    const roomList = createRoomListCache({
      cache,
      namespace: () => cacheNamespace(BASE_URL, token),
      enabled: () => true,
    })

    roomList.write([OPS])
    await settle()
    expect(await cache.keys('rooms')).toEqual([])

    token = 'tok-pasted'
    roomList.write([OPS])
    await settle()
    expect((await cache.keys('rooms')).length).toBe(1)
    expect((await roomList.read())?.length).toBe(1)
  })

  it('leaves a readable record when two tabs write back at once', async () => {
    // ADR 0085 settles concurrent tabs as last-writer-wins: both share one
    // database and both write after their own refreshes. What must never
    // happen is a *torn* record — a half-written list that reads back as
    // garbage — since every cached value is server-derived and a stale
    // overwrite costs at most one extra refresh.
    const cache = createMemoryCacheStore()
    const tabOne = harness({ cache }).roomList
    const tabTwo = harness({ cache }).roomList

    tabOne.write([OPS, LOUNGE])
    tabTwo.write([OPS])
    tabOne.write([OPS, LOUNGE])
    await settle()

    const rows = await harness({ cache }).roomList.read()
    // Whichever won, the record is one of the two whole lists, never a splice
    // of both, and exactly one record exists.
    expect(rows).not.toBeUndefined()
    expect([1, 2]).toContain(rows!.length)
    expect(rows!.every((room) => typeof room.room_id === 'string')).toBe(true)
    expect((await cache.keys('rooms')).length).toBe(1)
  })

  it("does not store one reader's rooms under the next reader's key", async () => {
    // A `/v1/rooms` request issued under token A can still be in flight when
    // the user signs out and pastes token B. Applying its result then writes
    // A's rooms under B's namespace — a durable cross-reader leak that the
    // sign-out wipe cannot catch, because the write lands after it.
    const cache = createMemoryCacheStore()
    let token: string | null = 'token-a'
    let release: (() => void) | undefined
    const held = new Promise<void>((resolve) => (release = resolve))
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, async () => {
        await held
        return HttpResponse.json({ data: [OPS, LOUNGE] })
      }),
    )
    const store = createRoomsStore(
      api(),
      memoryStorage(),
      createRoomListCache({
        cache,
        namespace: () => cacheNamespace(BASE_URL, token),
        enabled: () => true,
      }),
    )
    const refreshing = store.refresh()
    await settle()

    // Sign out, then a different reader signs in.
    store.resetSession()
    await cache.clear()
    token = 'token-b'

    release?.()
    await refreshing
    await settle()

    // Nothing of A's may be on disk under any key, and B's store must not be
    // showing A's rooms.
    expect(await cache.keys('rooms')).toEqual([])
    expect(store.rooms.value).toEqual([])
    // And B must still be able to load: the discarded response must not have
    // marked the list settled.
    expect(store.stale.value).toBe(false)
  })

  it('abandons an in-flight write when the cache is cleared underneath it', async () => {
    // `write()` checks `enabled()` and only then awaits the namespace digest,
    // so a lifecycle `clear()` — turning the setting off, or signing out — can
    // land in between and be overwritten by a write that was already on its
    // way. For a control whose promise is "turning this off erases what was
    // stored", that is a deletion barrier with a hole in it.
    const cache = createMemoryCacheStore()
    let on = true
    const roomList = createRoomListCache({
      cache,
      namespace: () => cacheNamespace(BASE_URL, 'tok-test'),
      enabled: () => on,
    })

    roomList.write([OPS, LOUNGE])
    // Same turn, exactly as `connectCacheSetting`'s effect fires it.
    on = false
    void cache.clear()
    await settle()

    expect(await cache.keys('rooms')).toEqual([])
  })

  it('prunes rooms the server no longer lists rather than merging', async () => {
    const { cache, roomList } = harness()
    roomList.write([OPS, LOUNGE])
    await settle()
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [OPS] }),
      ),
    )
    const store = createRoomsStore(
      api(),
      memoryStorage(),
      harness({ cache }).roomList,
    )
    await settle()
    await store.refresh()
    expect(store.rooms.value.map((room) => room.room_id)).toEqual(['!ops:hs'])
    expect((await roomList.read())?.map((room) => room.room_id)).toEqual([
      '!ops:hs',
    ])
  })

  it('does not let a late restore land on top of a settled refresh', async () => {
    const { cache } = harness()
    await cache.write('rooms', (await cacheNamespace(BASE_URL, 'tok-test'))!, {
      version: 1,
      savedAt: 0,
      rooms: [LOUNGE],
    })
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [OPS] }),
      ),
    )
    const store = createRoomsStore(
      api(),
      memoryStorage(),
      harness({ cache }).roomList,
    )
    // The network wins the race: the refresh settles before the cache read is
    // awaited to completion. Stale rows must not overwrite confirmed ones.
    await store.refresh()
    await settle()
    expect(store.rooms.value.map((room) => room.room_id)).toEqual(['!ops:hs'])
    expect(store.stale.value).toBe(false)
  })

  it('does not let a late restore land on top of a sign-out', async () => {
    const { cache } = harness()
    await cache.write('rooms', (await cacheNamespace(BASE_URL, 'tok-test'))!, {
      version: 1,
      savedAt: 0,
      rooms: [LOUNGE],
    })
    // Waiting on the read itself, not on a tick count: asserting "the rows did
    // not appear" is vacuously true while the read is still in flight, so a
    // fixed `settle()` here would pass against the unguarded `hydrate()` on
    // any machine slow enough.
    const inner = harness({ cache }).roomList
    let readSettled = () => {}
    const read = new Promise<void>((resolve) => (readSettled = resolve))
    const store = createRoomsStore(api(), memoryStorage(), {
      read: async () => {
        try {
          return await inner.read()
        } finally {
          readSettled()
        }
      },
      write: (rooms) => inner.write(rooms),
    })
    // Sign-out lands while the boot read is still in flight. It leaves exactly
    // the pristine state the hydrate guard treats as safe to write into —
    // no rows, unsettled — so only the session check can stop the previous
    // reader's rows from repainting the just-cleared list.
    store.resetSession()
    await read
    await settle()
    expect(store.rooms.value).toEqual([])
    expect(store.stale.value).toBe(false)
  })

  it('isolates readers: another token cannot read these rows', async () => {
    const { cache } = harness()
    harness({ cache, token: 'token-one' }).roomList.write([OPS])
    await settle()
    expect(await harness({ cache, token: 'token-two' }).roomList.read()).toBe(
      undefined,
    )
    expect(
      (await harness({ cache, token: 'token-one' }).roomList.read())?.length,
    ).toBe(1)
  })

  it('isolates servers: the same token against another base cannot read them', async () => {
    const cache = createMemoryCacheStore()
    createRoomListCache({
      cache,
      namespace: () => cacheNamespace(BASE_URL, 'tok-test'),
      enabled: () => true,
    }).write([OPS])
    await settle()
    const elsewhere = createRoomListCache({
      cache,
      namespace: () => cacheNamespace('http://other.test', 'tok-test'),
      enabled: () => true,
    })
    expect(await elsewhere.read()).toBeUndefined()
  })

  it('caches nothing without a token', async () => {
    const { cache, roomList } = harness({ token: null })
    roomList.write([OPS])
    await settle()
    expect(await cache.keys('rooms')).toEqual([])
    expect(await roomList.read()).toBeUndefined()
  })

  it('neither reads nor writes while the setting is off', async () => {
    const cache = createMemoryCacheStore()
    harness({ cache }).roomList.write([OPS])
    await settle()
    const disabled = harness({ cache, enabled: () => false }).roomList
    expect(await disabled.read()).toBeUndefined()
    disabled.write([OPS, LOUNGE])
    await settle()
    // The record on disk is the one the enabled cache wrote, untouched.
    expect((await harness({ cache }).roomList.read())?.length).toBe(1)
  })

  it('keeps exactly one record when the reader changes', async () => {
    const cache = createMemoryCacheStore()
    harness({ cache, token: 'token-one' }).roomList.write([OPS])
    await settle()
    harness({ cache, token: 'token-two' }).roomList.write([LOUNGE])
    await settle()
    expect((await cache.keys('rooms')).length).toBe(1)
    expect(await harness({ cache, token: 'token-one' }).roomList.read()).toBe(
      undefined,
    )
  })

  it('persists metadata only — no message text may reach disk', async () => {
    const cache = createMemoryCacheStore()
    // A future server field carrying message content must not ride along on
    // the strength of being present in the DTO.
    const withBody = {
      ...OPS,
      last_message_body: 'the secret plans',
    } as unknown as RoomDto
    harness({ cache }).roomList.write([withBody])
    await settle()
    const raw = JSON.stringify(
      await cache.read('rooms', (await cacheNamespace(BASE_URL, 'tok-test'))!),
    )
    expect(raw).not.toContain('the secret plans')
    expect(raw).toContain('Ops')
  })

  it('keeps a row whose unread counts are missing or nonsense', async () => {
    const cache = createMemoryCacheStore()
    const uncounted = {
      account_id: ACCOUNT,
      account_user_id: '@me:example.org',
      room_id: '!uncounted:hs',
      name: 'Uncounted',
      last_activity_ts: 10,
    } as unknown as RoomDto
    harness({ cache }).roomList.write([uncounted])
    await settle()
    const rows = await harness({ cache }).roomList.read()
    // Rejecting these would discard the whole list on any server or fixture
    // that leaves the counts off — which is how the e2e lane first failed.
    expect(rows?.length).toBe(1)
    expect(rows?.[0].notification_count).toBe(0)
  })

  it('ignores a malformed record rather than painting junk', async () => {
    const cache = createMemoryCacheStore()
    const key = (await cacheNamespace(BASE_URL, 'tok-test'))!
    await cache.write('rooms', key, { version: 99, rooms: [OPS] })
    expect(await harness({ cache }).roomList.read()).toBeUndefined()
    await cache.write('rooms', key, {
      version: 1,
      savedAt: 0,
      rooms: [{ room_id: '!broken:hs' }, OPS],
    })
    // The sound row survives; the unusable one is dropped.
    expect((await harness({ cache }).roomList.read())?.length).toBe(1)
  })
})

describe('ensureLoaded', () => {
  it('refreshes once a session, even over a restored list', async () => {
    let calls = 0
    server.use(
      http.get(`${BASE_URL}/v1/rooms`, () => {
        calls += 1
        return HttpResponse.json({ data: [OPS] })
      }),
    )
    const { cache, roomList } = harness()
    roomList.write([OPS, LOUNGE])
    await settle()
    const store = createRoomsStore(
      api(),
      memoryStorage(),
      harness({ cache }).roomList,
    )
    await waitForRows(store)
    // Restored: `loading` is false and the list is non-empty, so the guards
    // this replaced would both have decided the list was already loaded.
    expect(store.loading.value).toBe(false)
    expect(store.rooms.value.length).toBe(2)

    await store.ensureLoaded()
    expect(calls).toBe(1)
    expect(store.rooms.value.map((room) => room.room_id)).toEqual(['!ops:hs'])

    await store.ensureLoaded()
    expect(calls).toBe(1)
  })
})
