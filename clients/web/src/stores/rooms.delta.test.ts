/**
 * Incremental room-list refreshes (ADR 0134): the store sends the cursor of
 * its last read as `since`, merges the delta into the server's list it holds,
 * falls back to a full read when it cannot trust the result, and persists the
 * server's list with its cursor so a cold start's first refresh is a delta.
 */
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createApiClient } from '../api/client'
import { memoryStorage } from '../test/memory-storage'
import { cacheNamespace, createMemoryCacheStore } from './cache-store'
import { createRoomListCache, type RoomListServerView } from './room-list-cache'
import type { RoomDto } from './room-list'
import { createRoomsStore } from './rooms'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'

function room(
  room_id: string,
  last_activity_ts: number,
  extra: Partial<RoomDto> = {},
): RoomDto {
  return {
    account_id: ACCOUNT,
    account_user_id: '@me:example.org',
    room_id,
    name: room_id.slice(1),
    last_activity_ts,
    notification_count: 0,
    highlight_count: 0,
    is_direct: false,
    ...extra,
  } as RoomDto
}

const A = room('!a:hs', 300)
const B = room('!b:hs', 200)
const C = room('!c:hs', 100)

interface ListBody {
  data: RoomDto[]
  full?: boolean
  removed?: { account_id: string; room_id: string }[]
  total?: number
  cursor?: string
}

const full = (rooms: RoomDto[], cursor: string): ListBody => ({
  data: rooms,
  full: true,
  removed: [],
  total: rooms.length,
  cursor,
})

const delta = (
  rooms: RoomDto[],
  total: number,
  cursor: string,
  removed: string[] = [],
): ListBody => ({
  data: rooms,
  full: false,
  removed: removed.map((room_id) => ({ account_id: ACCOUNT, room_id })),
  total,
  cursor,
})

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

/**
 * Serve `/v1/rooms` from a script: each request takes the next reply, and the
 * `since` it carried (or `null`) is recorded.
 */
function serveRooms(replies: (ListBody | number)[]) {
  const sinces: (string | null)[] = []
  server.use(
    http.get(`${BASE_URL}/v1/rooms`, ({ request }) => {
      sinces.push(new URL(request.url).searchParams.get('since'))
      const reply = replies.shift()
      if (reply === undefined) {
        throw new Error('unexpected /v1/rooms request')
      }
      if (typeof reply === 'number') {
        return HttpResponse.json(
          { error: { code: 'bad_request', message: 'scripted' } },
          { status: reply },
        )
      }
      return HttpResponse.json(reply)
    }),
  )
  return sinces
}

const api = () =>
  createApiClient(
    {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )

function harness(
  cache = createMemoryCacheStore(),
  enabled: () => boolean = () => true,
) {
  const roomList = createRoomListCache({
    cache,
    namespace: () => cacheNamespace(BASE_URL, 'tok-test'),
    enabled,
  })
  return { cache, roomList }
}

/** Wait for a fire-and-forget server-view write to land, then return it. */
async function waitForView(
  roomList: ReturnType<typeof harness>['roomList'],
  until: (view: RoomListServerView | undefined) => boolean,
): Promise<RoomListServerView | undefined> {
  for (let i = 0; i < 200; i += 1) {
    const view = await roomList.readServerView()
    if (until(view)) return view
    await new Promise((resolve) => setTimeout(resolve, 5))
  }
  return roomList.readServerView()
}

const settle = async () => {
  for (let i = 0; i < 3; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 0))
  }
}

const shown = (store: ReturnType<typeof createRoomsStore>) =>
  store.rooms.value.map((r) => r.room_id)

describe('incremental room-list refresh (ADR 0134)', () => {
  it('sends the last cursor and merges the delta into the list', async () => {
    const sinces = serveRooms([
      full([A, B, C], 'c1'),
      delta([room('!c:hs', 400, { name: 'C!' })], 2, 'c2', ['!b:hs']),
      delta([], 2, 'c3'),
    ])
    const store = createRoomsStore(api(), memoryStorage(), harness().roomList)
    await store.refresh()
    expect(shown(store)).toEqual(['!a:hs', '!b:hs', '!c:hs'])

    await store.refresh()
    expect(shown(store)).toEqual(['!c:hs', '!a:hs'])
    expect(store.rooms.value[0].name).toBe('C!')

    await store.refresh()
    expect(sinces).toEqual([null, 'c1', 'c2'])
    expect(shown(store)).toEqual(['!c:hs', '!a:hs'])
  })

  it('reads in full when the merged list does not hold `total` rooms', async () => {
    // `!b` was purged server-side: the delta cannot name it, but the count
    // drops, so the client cannot trust its merge.
    const sinces = serveRooms([
      full([A, B, C], 'c1'),
      delta([], 2, 'c2'),
      full([A, C], 'c3'),
      delta([], 2, 'c4'),
    ])
    const store = createRoomsStore(api(), memoryStorage(), harness().roomList)
    await store.refresh()
    await store.refresh()
    expect(sinces).toEqual([null, 'c1', null])
    expect(shown(store)).toEqual(['!a:hs', '!c:hs'])
    expect(store.error.value).toBeNull()

    await store.refresh()
    expect(sinces.at(-1)).toBe('c3')
  })

  it('drops a cursor the server refuses with a 400 and reads in full once', async () => {
    const { roomList } = harness()
    const sinces = serveRooms([full([A, B], 'c1'), 400, full([A], 'c2')])
    const store = createRoomsStore(api(), memoryStorage(), roomList)
    await store.refresh()
    await waitForView(roomList, (view) => view?.cursor === 'c1')

    await store.refresh()
    expect(sinces).toEqual([null, 'c1', null])
    expect(shown(store)).toEqual(['!a:hs'])
    expect(store.error.value).toBeNull()
    // The record that came back is the full read's, not the refused cursor.
    const view = await waitForView(roomList, (v) => v?.cursor === 'c2')
    expect(view?.cursor).toBe('c2')
  })

  it('keeps the cursor across a 5xx, which a retry with the same `since` can fix', async () => {
    const sinces = serveRooms([
      full([A, B], 'c1'),
      503,
      delta([room('!b:hs', 500)], 2, 'c2'),
    ])
    const store = createRoomsStore(api(), memoryStorage(), harness().roomList)
    await store.refresh()
    await store.refresh()
    expect(store.error.value).not.toBeNull()
    expect(shown(store)).toEqual(['!a:hs', '!b:hs'])

    await store.refresh()
    expect(sinces).toEqual([null, 'c1', 'c1'])
    expect(shown(store)).toEqual(['!b:hs', '!a:hs'])
  })

  it('treats a server with no cursor as always full, and never sends `since`', async () => {
    const sinces = serveRooms([{ data: [A, B] }, { data: [A] }])
    const store = createRoomsStore(api(), memoryStorage(), harness().roomList)
    await store.refresh()
    await store.refresh()
    expect(sinces).toEqual([null, null])
    expect(shown(store)).toEqual(['!a:hs'])
  })

  it('restores the persisted list and cursor, so a cold start begins with a delta', async () => {
    const { cache, roomList } = harness()
    serveRooms([full([A, B, C], 'c1')])
    const first = createRoomsStore(api(), memoryStorage(), roomList)
    await first.refresh()
    await waitForView(roomList, (view) => view?.cursor === 'c1')

    // A new launch: a fresh store over the same disk.
    const sinces = serveRooms([delta([room('!a:hs', 900)], 3, 'c2')])
    const second = createRoomsStore(
      api(),
      memoryStorage(),
      harness(cache).roomList,
    )
    await second.refresh()
    expect(sinces).toEqual(['c1'])
    expect(shown(second)).toEqual(['!a:hs', '!b:hs', '!c:hs'])
    expect(second.rooms.value[0].last_activity_ts).toBe(900)
  })

  it('leaves the disk alone when a delta only repeats rows already held', async () => {
    const { roomList } = harness()
    serveRooms([
      full([A, B], 'c1'),
      delta([A], 2, 'c2'),
      delta([room('!b:hs', 700)], 2, 'c3'),
    ])
    const store = createRoomsStore(api(), memoryStorage(), roomList)
    await store.refresh()
    await waitForView(roomList, (view) => view?.cursor === 'c1')

    await store.refresh()
    await settle()
    expect((await roomList.readServerView())?.cursor).toBe('c1')

    await store.refresh()
    const view = await waitForView(roomList, (v) => v?.cursor === 'c3')
    expect(view?.rooms.map((r) => r.room_id)).toEqual(['!b:hs', '!a:hs'])
  })

  it('saves the list once the cache is turned on, even if the delta changed nothing', async () => {
    // The full read lands while the cache is off, so nothing is saved, and
    // every delta after the setting is turned on repeats rows already held.
    let on = false
    const { roomList } = harness(createMemoryCacheStore(), () => on)
    serveRooms([full([A, B], 'c1'), delta([], 2, 'c2'), delta([], 2, 'c3')])
    const store = createRoomsStore(api(), memoryStorage(), roomList)
    await store.refresh()
    await settle()
    on = true
    expect(await roomList.readServerView()).toBeUndefined()

    await store.refresh()
    const view = await waitForView(roomList, (v) => v !== undefined)
    expect(view?.cursor).toBe('c2')
    expect(view?.rooms.map((r) => r.room_id)).toEqual(['!a:hs', '!b:hs'])

    // Saved once; the next unchanged delta leaves it alone again.
    await store.refresh()
    await settle()
    expect((await roomList.readServerView())?.cursor).toBe('c2')
  })

  it('saves the list again after the cache is cleared mid-session', async () => {
    const { cache, roomList } = harness()
    serveRooms([full([A, B], 'c1'), delta([], 2, 'c2')])
    const store = createRoomsStore(api(), memoryStorage(), roomList)
    await store.refresh()
    await waitForView(roomList, (view) => view?.cursor === 'c1')

    // The setting turned off and back on: `connectCacheSetting` clears.
    await cache.clear()
    expect(await roomList.readServerView()).toBeUndefined()
    await store.refresh()
    const view = await waitForView(roomList, (v) => v !== undefined)
    expect(view?.cursor).toBe('c2')
  })

  it('persists only what the server said, never a live patch', async () => {
    const { roomList } = harness()
    serveRooms([
      full([A, B], 'c1'),
      delta([room('!b:hs', 200, { name: 'B2' })], 2, 'c2'),
    ])
    const store = createRoomsStore(api(), memoryStorage(), roomList, {
      cacheWriteDelayMs: 0,
    })
    await store.refresh()
    await waitForView(roomList, (view) => view?.cursor === 'c1')

    // A live rename, and new activity on the newest row, which the store
    // patches in place.
    store.noteDisplayName(ACCOUNT, '!b:hs', 'Live B')
    store.noteActivity(ACCOUNT, '!a:hs', 950)
    expect(
      store.rooms.value.find((r) => r.room_id === '!b:hs')?.display_name,
    ).toBe('Live B')
    expect(store.rooms.value[0].last_activity_ts).toBe(950)

    // The next delta changes `!b` only, so the server list is written again,
    // and it must hold the server's values for both rooms.
    await store.refresh()
    const view = await waitForView(roomList, (v) => v?.cursor === 'c2')
    const byId = (id: string) => view?.rooms.find((r) => r.room_id === id)
    expect(byId('!a:hs')?.last_activity_ts).toBe(300)
    expect(byId('!b:hs')?.display_name).toBeNull()
    expect(byId('!b:hs')?.name).toBe('B2')
  })

  it('forgets the server list on sign-out, so the next reader starts with a full read', async () => {
    const sinces = serveRooms([full([A], 'c1'), full([B], 'c2')])
    const store = createRoomsStore(api(), memoryStorage(), null)
    await store.refresh()
    store.resetSession()
    await store.refresh()
    expect(sinces).toEqual([null, null])
    expect(shown(store)).toEqual(['!b:hs'])
  })

  it('keeps the server list in memory with the cache off', async () => {
    const sinces = serveRooms([full([A, B], 'c1'), delta([], 2, 'c2')])
    const store = createRoomsStore(api(), memoryStorage(), null)
    await store.refresh()
    await store.refresh()
    expect(sinces).toEqual([null, 'c1'])
    expect(shown(store)).toEqual(['!a:hs', '!b:hs'])
  })

  it('writes an unchanged view after a drop, since nothing is saved any more', async () => {
    const { roomList } = harness()
    roomList.writeServerView({ cursor: 'c1', rooms: [A] })
    roomList.forgetServerView()
    roomList.writeServerView(
      { cursor: 'c2', rooms: [A] },
      { unlessSaved: true },
    )
    expect((await roomList.readServerView())?.cursor).toBe('c2')
  })

  it('never lets the drop of a refused cursor land after the record that replaces it', async () => {
    // A drop slower than the write that follows it: without ordering, the
    // drop would delete the new record.
    const base = createMemoryCacheStore()
    const slowDrops: typeof base = Object.assign(Object.create(base), {
      drop: async (...args: Parameters<typeof base.drop>) => {
        await new Promise((resolve) => setTimeout(resolve, 30))
        return base.drop(...args)
      },
    })
    const { roomList } = harness(slowDrops)
    roomList.writeServerView({ cursor: 'old', rooms: [A] })
    roomList.forgetServerView()
    roomList.writeServerView({ cursor: 'new', rooms: [B] })
    // Past the slow drop, so an unordered one has had time to land.
    await new Promise((resolve) => setTimeout(resolve, 80))
    const view = await roomList.readServerView()
    expect(view?.cursor).toBe('new')
  })
})
