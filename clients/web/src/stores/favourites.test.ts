import { signal, type Signal } from '@preact/signals'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import {
  afterAll,
  afterEach,
  beforeAll,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from 'vitest'
import { createApiClient } from '../api/client'
import {
  ACCOUNT_DATA_CHANGED,
  type AccountDataChange,
  type LiveFrame,
} from '../api/frames'
import type { LiveConnection } from './live-connection'
import {
  FAVOURITE_TAG,
  createFavouriteStore,
  migrationAssignments,
  ordersForInsertion,
  parseRoomTags,
  pinToTopAssignments,
  planFavouriteMove,
  roomIdsInDirectMap,
  type FavouriteRoomList,
  type FavouriteStore,
  type RoomTag,
} from './favourites'
import { roomKey, type RoomDto } from './room-list'

const BASE_URL = 'http://axon.test'
const TAG_URL = `${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/tags/:tag`
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const OTHER = '6b53f7f0-0000-4000-8000-000000000002'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

interface RecordedTag {
  method: 'PUT' | 'DELETE'
  accountId: string
  roomId: string
  tag: string
  order?: number
}

const recorded: RecordedTag[] = []

beforeEach(() => {
  recorded.length = 0
  server.use(
    http.put(TAG_URL, async ({ params, request }) => {
      const body = (await request.json()) as { order?: number }
      recorded.push({
        method: 'PUT',
        accountId: String(params.accountId),
        roomId: String(params.roomId),
        tag: String(params.tag),
        order: body.order,
      })
      return HttpResponse.json({ data: {} })
    }),
    http.delete(TAG_URL, ({ params }) => {
      recorded.push({
        method: 'DELETE',
        accountId: String(params.accountId),
        roomId: String(params.roomId),
        tag: String(params.tag),
      })
      return HttpResponse.json({ data: {} })
    }),
  )
})

function room(overrides: Partial<RoomDto> & { room_id: string }): RoomDto {
  return {
    account_id: ACCOUNT,
    account_user_id: '@me:example.org',
    last_activity_ts: 0,
    highlight_count: 0,
    notification_count: 0,
    is_direct: false,
    tags: [],
    ...overrides,
  }
}

function keyOf(entry: Pick<RoomDto, 'account_id' | 'room_id'>): string {
  return roomKey(entry)
}

function orderOf(rooms: readonly RoomDto[], key: string): number | null {
  const tags = rooms.find((entry) => keyOf(entry) === key)?.tags
  const order = tags?.find((tag) => tag.name === FAVOURITE_TAG)?.order
  return typeof order === 'number' ? order : null
}

interface Harness {
  store: FavouriteStore
  rooms: Signal<RoomDto[]>
  confirmed: Signal<boolean>
  stale: Signal<boolean>
  loading: Signal<boolean>
  pinnedRooms: Signal<string[]>
  accounts: Signal<readonly { account_id: string; state: string }[]>
  accountsLoading: Signal<boolean>
  accountsError: Signal<string | null>
  reconnects: Signal<number>
  refreshes: () => number
  emit(accountId: string, change: AccountDataChange): void
}

function harness(initial: RoomDto[] = [], pinned: string[] = []): Harness {
  const rooms = signal<RoomDto[]>(initial)
  const confirmed = signal(false)
  const stale = signal(false)
  const loading = signal(false)
  const pinnedRooms = signal<string[]>(pinned)
  const accounts = signal<readonly { account_id: string; state: string }[]>([
    { account_id: ACCOUNT, state: 'active' },
    { account_id: OTHER, state: 'active' },
  ])
  const accountsLoading = signal(false)
  const accountsError = signal<string | null>(null)
  let seq = 0
  const generation = new Map<string, number>()
  let refreshCount = 0
  const listeners = new Set<(frame: LiveFrame) => void>()

  function assignRoomTags(key: string, tags: RoomTag[]): number {
    const index = rooms.peek().findIndex((entry) => keyOf(entry) === key)
    if (index === -1) {
      return 0
    }
    seq += 1
    generation.set(key, seq)
    rooms.value = rooms
      .peek()
      .map((entry, i) => (i === index ? { ...entry, tags } : entry))
    return seq
  }

  const list: FavouriteRoomList = {
    rooms,
    confirmed,
    stale,
    loading,
    peekRooms: () => rooms.peek(),
    assignRoomTags,
    restoreRoomTags(key, tagGeneration, tags) {
      if (generation.get(key) !== tagGeneration) {
        return false
      }
      const index = rooms.peek().findIndex((entry) => keyOf(entry) === key)
      if (index === -1) {
        return true
      }
      rooms.value = rooms
        .peek()
        .map((entry, i) =>
          i === index ? { ...entry, tags: tags ?? [] } : entry,
        )
      return true
    },
    applyAccountData(accountId, change) {
      if (change.eventType === 'm.tag') {
        if (change.roomId === null) {
          return
        }
        assignRoomTags(
          `${accountId}/${change.roomId}`,
          parseRoomTags(change.content),
        )
        return
      }
      if (change.eventType !== 'm.direct') {
        return
      }
      const ids = roomIdsInDirectMap(change.content)
      rooms.value = rooms
        .peek()
        .map((entry) =>
          entry.account_id === accountId
            ? { ...entry, is_direct: ids.has(entry.room_id) }
            : entry,
        )
    },
    refresh: async () => {
      refreshCount += 1
    },
  }

  const reconnects = signal(0)
  const live = {
    reconnects,
    subscribe(listener: (frame: LiveFrame) => void) {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
  } as unknown as LiveConnection
  const api = createApiClient(
    {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
  const store = createFavouriteStore({
    api,
    live,
    rooms: list,
    settings: { pinnedRooms },
    accounts: { accounts, loading: accountsLoading, error: accountsError },
  })

  return {
    store,
    rooms,
    confirmed,
    stale,
    loading,
    pinnedRooms,
    accounts,
    accountsLoading,
    accountsError,
    reconnects,
    refreshes: () => refreshCount,
    emit(accountId, change) {
      const payload: Record<string, unknown> = {
        event_type: change.eventType,
        content: change.content,
      }
      if (change.roomId !== null) {
        payload.room_id = change.roomId
      }
      listeners.forEach((listener) =>
        listener({
          type: ACCOUNT_DATA_CHANGED,
          accountId,
          payload,
        }),
      )
    },
  }
}

function ready(env: Harness): void {
  env.confirmed.value = true
  env.stale.value = false
  env.loading.value = false
  env.accountsLoading.value = false
}

describe('parseRoomTags', () => {
  it('reads orders and drops a malformed content object', () => {
    expect(parseRoomTags(null)).toEqual([])
    expect(parseRoomTags({ tags: [] })).toEqual([])
    expect(
      parseRoomTags({
        tags: {
          'm.favourite': { order: 0.25 },
          'u.custom': {},
          'u.none': { order: null },
          'u.bad': { order: 'nope' },
        },
      }),
    ).toEqual([
      { name: 'm.favourite', order: 0.25 },
      { name: 'u.custom' },
      { name: 'u.none', order: null },
      { name: 'u.bad' },
    ])
  })
})

describe('roomIdsInDirectMap', () => {
  it('collects room ids from every array value', () => {
    expect(
      roomIdsInDirectMap({
        '@a:hs': ['!r:hs', 1],
        '@b:hs': '!not-array',
      }),
    ).toEqual(new Set(['!r:hs']))
    expect(roomIdsInDirectMap(null)).toEqual(new Set())
  })
})

describe('pinToTopAssignments', () => {
  it('uses 0.5, then half the minimum, then a rebalance', () => {
    const alone = room({ room_id: '!a:hs' })
    expect(pinToTopAssignments([alone], keyOf(alone))).toEqual([
      { key: keyOf(alone), order: 0.5 },
    ])

    const low = room({
      room_id: '!low:hs',
      tags: [{ name: FAVOURITE_TAG, order: 0.4 }],
    })
    const target = room({ room_id: '!top:hs' })
    expect(pinToTopAssignments([low, target], keyOf(target))).toEqual([
      { key: keyOf(target), order: 0.2 },
    ])

    const packed = room({
      room_id: '!packed:hs',
      tags: [{ name: FAVOURITE_TAG, order: 0 }],
    })
    expect(pinToTopAssignments([packed, target], keyOf(target))).toEqual([
      { key: keyOf(target), order: 1 / 3 },
      { key: keyOf(packed), order: 2 / 3 },
    ])
  })
})

describe('migrationAssignments', () => {
  it('spaces one account as (index + 1) / (n + 1) and another as 0.5', () => {
    const first = `${ACCOUNT}/!a:hs`
    const second = `${ACCOUNT}/!b:hs`
    const other = `${OTHER}/!c:hs`
    expect(migrationAssignments([first, second, other])).toEqual([
      { key: first, order: 1 / 3 },
      { key: second, order: 2 / 3 },
      { key: other, order: 0.5 },
    ])
  })
})

describe('planFavouriteMove', () => {
  const fav = (key: string) => key.startsWith('fav')
  const orders = new Map<string, number>([
    ['fav-a', 0.2],
    ['fav-b', 0.8],
  ])
  const lookup = {
    orderOf: (key: string) => orders.get(key) ?? null,
    isFavourite: fav,
  }

  it('inserts at the midpoint inside the favourite prefix', () => {
    expect(
      planFavouriteMove('fav-b', 0, ['fav-a', 'fav-b', 'plain'], 2, lookup),
    ).toEqual({
      kind: 'assign',
      assignments: [{ key: 'fav-b', order: 0.1 }],
    })
  })

  it('unpins a favourite dropped on or below the separator', () => {
    expect(
      planFavouriteMove('fav-a', 1, ['fav-a', 'plain'], 1, lookup),
    ).toEqual({ kind: 'unpin', key: 'fav-a' })
  })

  it('drops a leftover local pin without a tag write', () => {
    expect(
      planFavouriteMove('local', 1, ['local', 'plain'], 1, lookup),
    ).toEqual({ kind: 'drop-local', key: 'local' })
  })

  it('leaves a drag among unpinned rooms alone', () => {
    expect(
      planFavouriteMove('plain-a', 1, ['plain-a', 'plain-b'], 0, lookup),
    ).toEqual({ kind: 'noop' })
  })

  it('is a no-op when the last favourite has nowhere below it to land', () => {
    expect(
      planFavouriteMove('fav-b', 2, ['fav-a', 'fav-b'], 2, lookup),
    ).toEqual({ kind: 'noop' })
  })
})

describe('ordersForInsertion', () => {
  it('rebalances the visible prefix when the gap is below epsilon', () => {
    const orders = new Map<string, number>([
      ['a', 0.5],
      ['b', 0.5],
      ['c', 0.5 + 1e-12],
    ])
    expect(
      ordersForInsertion(['a', 'b', 'c'], 1, (key) => orders.get(key) ?? null),
    ).toEqual([
      { key: 'a', order: 1 / 4 },
      // `b` is already `(1 + 1) / (3 + 1)`.
      { key: 'c', order: 3 / 4 },
    ])
  })

  it('rebalances a drag to the top when the next order is already 0', () => {
    expect(
      ordersForInsertion(['x', 'a'], 0, (key) => (key === 'a' ? 0 : null)),
    ).toEqual([
      { key: 'x', order: 1 / 3 },
      { key: 'a', order: 2 / 3 },
    ])
  })

  it('rebalances an insert at the end when the previous order is already 1', () => {
    expect(
      ordersForInsertion(['a', 'b'], 1, (key) => (key === 'a' ? 1 : null)),
    ).toEqual([
      { key: 'a', order: 1 / 3 },
      { key: 'b', order: 2 / 3 },
    ])
  })

  it('leaves a lone room at order 0', () => {
    expect(ordersForInsertion(['a'], 0, () => null)).toEqual([
      { key: 'a', order: 0 },
    ])
  })
})

describe('createFavouriteStore', () => {
  it('does not upload local pins until start', async () => {
    const pinned = `${ACCOUNT}/!a:hs`
    const env = harness([room({ room_id: '!a:hs' })], [pinned])
    ready(env)

    await new Promise((resolve) => setTimeout(resolve, 20))

    expect(recorded).toEqual([])
    expect(env.pinnedRooms.value).toEqual([pinned])
  })

  it('pins to the top with order 0.5 and leaves pinnedRooms untouched', async () => {
    const entry = room({ room_id: '!a:hs' })
    const env = harness([entry])
    ready(env)

    expect(env.store.pin(keyOf(entry))).toBe(true)

    await vi.waitFor(() => expect(recorded).toHaveLength(1))
    expect(recorded[0]).toMatchObject({
      method: 'PUT',
      accountId: ACCOUNT,
      roomId: '!a:hs',
      tag: FAVOURITE_TAG,
      order: 0.5,
    })
    expect(orderOf(env.rooms.value, keyOf(entry))).toBe(0.5)
    expect(env.pinnedRooms.value).toEqual([])
  })

  it('puts a pin after a refresh drops the favourite the last flush wrote', async () => {
    const entry = room({ room_id: '!a:hs' })
    const key = keyOf(entry)
    const env = harness([entry])
    ready(env)

    env.store.pin(key)
    await vi.waitFor(() => expect(recorded).toHaveLength(1))
    expect(recorded[0]?.order).toBe(0.5)

    env.rooms.value = env.rooms.value.map((row) =>
      keyOf(row) === key ? { ...row, tags: [] } : row,
    )
    env.store.pin(key)

    await vi.waitFor(() => expect(recorded).toHaveLength(2))
    expect(recorded[1]).toMatchObject({ method: 'PUT', order: 0.5 })
    expect(orderOf(env.rooms.value, key)).toBe(0.5)
  })

  it('puts a pin after a refresh drops a favourite a tag frame recorded', async () => {
    const entry = room({ room_id: '!a:hs' })
    const key = keyOf(entry)
    const env = harness([entry])

    env.emit(ACCOUNT, {
      roomId: '!a:hs',
      eventType: 'm.tag',
      content: { tags: { 'm.favourite': { order: 0.5 } } },
    })
    expect(orderOf(env.rooms.value, key)).toBe(0.5)

    env.rooms.value = env.rooms.value.map((row) =>
      keyOf(row) === key ? { ...row, tags: [] } : row,
    )
    env.store.pin(key)

    await vi.waitFor(() => expect(recorded).toHaveLength(1))
    expect(recorded[0]).toMatchObject({ method: 'PUT', order: 0.5 })
    expect(orderOf(env.rooms.value, key)).toBe(0.5)
  })

  it('restores tags and reports the error when the PUT fails', async () => {
    server.use(
      http.put(TAG_URL, () =>
        HttpResponse.json(
          { error: { code: 'upstream', message: 'no' } },
          { status: 500 },
        ),
      ),
    )
    const entry = room({ room_id: '!a:hs', tags: [] })
    const env = harness([entry])
    ready(env)

    env.store.pin(keyOf(entry))

    await vi.waitFor(() =>
      expect(env.store.error.value).toBe('Could not update favorites'),
    )
    expect(env.rooms.value[0]?.tags).toEqual([])
  })

  it('uploads local pins once and then clears them', async () => {
    const first = room({ room_id: '!a:hs' })
    const second = room({ room_id: '!b:hs' })
    const other = room({ room_id: '!c:hs', account_id: OTHER })
    const env = harness(
      [first, second, other],
      [keyOf(first), keyOf(second), keyOf(other)],
    )
    ready(env)

    env.store.start()

    await vi.waitFor(() => expect(env.pinnedRooms.value).toEqual([]))
    expect(recorded.map((call) => [call.roomId, call.order])).toEqual([
      ['!a:hs', 1 / 3],
      ['!b:hs', 2 / 3],
      ['!c:hs', 0.5],
    ])
  })

  it('discards local pins when the homeserver already has a favourite', async () => {
    const fav = room({
      room_id: '!fav:hs',
      tags: [{ name: FAVOURITE_TAG, order: 0.4 }],
    })
    const plain = room({ room_id: '!plain:hs' })
    const env = harness([fav, plain], [keyOf(plain)])
    ready(env)

    env.store.start()

    await vi.waitFor(() => expect(env.pinnedRooms.value).toEqual([]))
    expect(recorded).toEqual([])
    expect(orderOf(env.rooms.value, keyOf(fav))).toBe(0.4)
  })

  it('keeps a failed migration pin and retries after a backoff', async () => {
    let attempts = 0
    server.use(
      http.put(TAG_URL, async ({ params, request }) => {
        attempts += 1
        if (attempts === 1) {
          return HttpResponse.json(
            { error: { code: 'upstream', message: 'no' } },
            { status: 500 },
          )
        }
        const body = (await request.json()) as { order?: number }
        recorded.push({
          method: 'PUT',
          accountId: String(params.accountId),
          roomId: String(params.roomId),
          tag: String(params.tag),
          order: body.order,
        })
        return HttpResponse.json({ data: {} })
      }),
    )
    const entry = room({ room_id: '!a:hs' })
    const pinned = keyOf(entry)
    const env = harness([entry], [pinned])
    ready(env)

    env.store.start()

    await vi.waitFor(() =>
      expect(env.store.error.value).toBe('Could not update favorites'),
    )
    env.stale.value = true
    env.stale.value = false
    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(attempts).toBe(1)
    expect(env.pinnedRooms.value).toEqual([pinned])
    expect(env.rooms.value[0]?.tags).toEqual([])

    await vi.waitFor(() => expect(attempts).toBe(2), { timeout: 2500 })
    expect(recorded[0]?.order).toBe(0.5)
    expect(env.pinnedRooms.value).toEqual([])
  })

  it('reports that a pin did nothing when the room is not listed', () => {
    const env = harness([])
    expect(env.store.pin(`${ACCOUNT}/!missing:hs`)).toBe(false)
    expect(recorded).toEqual([])
  })

  it('sends the unstar that lands while the pin PUT is in flight', async () => {
    let release: () => void = () => {}
    const gate = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.put(TAG_URL, async ({ params, request }) => {
        const body = (await request.json()) as { order?: number }
        recorded.push({
          method: 'PUT',
          accountId: String(params.accountId),
          roomId: String(params.roomId),
          tag: String(params.tag),
          order: body.order,
        })
        await gate
        return HttpResponse.json({ data: {} })
      }),
    )
    const entry = room({ room_id: '!a:hs' })
    const env = harness([entry])

    env.store.pin(keyOf(entry))
    await vi.waitFor(() => expect(recorded).toHaveLength(1))
    env.store.unpin(keyOf(entry))
    expect(env.rooms.value[0]?.tags).toEqual([])

    release()

    await vi.waitFor(() =>
      expect(recorded.some((call) => call.method === 'DELETE')).toBe(true),
    )
    expect(env.refreshes()).toBe(0)

    env.emit(ACCOUNT, {
      roomId: '!a:hs',
      eventType: 'm.tag',
      content: { tags: { 'm.favourite': { order: 0.2 } } },
    })
    expect(orderOf(env.rooms.value, keyOf(entry))).toBe(0.2)
    expect(env.refreshes()).toBe(0)
  })

  it('refreshes tags after the socket reconnects once migration is armed', async () => {
    const entry = room({ room_id: '!a:hs' })
    const env = harness([entry])
    ready(env)

    env.reconnects.value = 1
    await new Promise((resolve) => setTimeout(resolve, 20))
    expect(env.refreshes()).toBe(0)

    env.store.start()
    await new Promise((resolve) => setTimeout(resolve, 20))
    expect(env.refreshes()).toBe(0)

    env.reconnects.value = 2
    await vi.waitFor(() => expect(env.refreshes()).toBe(1))
  })

  it('removes a not-yet-favourite local pin without a DELETE', () => {
    const entry = room({ room_id: '!a:hs' })
    const env = harness([entry], [keyOf(entry)])

    env.store.unpin(keyOf(entry))

    expect(env.pinnedRooms.value).toEqual([])
    expect(recorded).toEqual([])
  })

  it('sends one PUT for a burst of debounced keyboard moves', async () => {
    const first = room({
      room_id: '!a:hs',
      tags: [{ name: FAVOURITE_TAG, order: 0.2 }],
    })
    const second = room({
      room_id: '!b:hs',
      tags: [{ name: FAVOURITE_TAG, order: 0.8 }],
    })
    const env = harness([first, second])
    const firstKey = keyOf(first)
    const secondKey = keyOf(second)

    env.store.move(secondKey, 0, [firstKey, secondKey], 2, { debounce: true })
    env.store.move(secondKey, 1, [secondKey, firstKey], 2, { debounce: true })

    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(recorded).toEqual([])

    await vi.waitFor(() => expect(recorded).toHaveLength(1))
    expect(recorded[0]).toMatchObject({
      method: 'PUT',
      roomId: '!b:hs',
      order: (0.2 + 1) / 2,
    })
  })

  it('applies a tag frame immediately and refreshes one ignored during a write', async () => {
    let release: () => void = () => {}
    const gate = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.put(TAG_URL, async ({ params, request }) => {
        const body = (await request.json()) as { order?: number }
        recorded.push({
          method: 'PUT',
          accountId: String(params.accountId),
          roomId: String(params.roomId),
          tag: String(params.tag),
          order: body.order,
        })
        await gate
        return HttpResponse.json({ data: {} })
      }),
    )
    const pending = room({ room_id: '!a:hs' })
    const other = room({ room_id: '!b:hs' })
    const env = harness([pending, other])

    env.store.pin(keyOf(pending))
    await vi.waitFor(() => expect(recorded).toHaveLength(1))

    env.emit(ACCOUNT, {
      roomId: '!b:hs',
      eventType: 'm.tag',
      content: { tags: { 'm.favourite': { order: 0.3 } } },
    })
    env.emit(ACCOUNT, {
      roomId: '!a:hs',
      eventType: 'm.tag',
      content: { tags: { 'm.favourite': { order: 0.9 } } },
    })
    env.emit(ACCOUNT, {
      roomId: null,
      eventType: 'm.direct',
      content: { '@bob:example.org': ['!b:hs'] },
    })

    expect(orderOf(env.rooms.value, keyOf(other))).toBe(0.3)
    expect(orderOf(env.rooms.value, keyOf(pending))).toBe(0.5)
    expect(
      env.rooms.value.find((entry) => entry.room_id === '!b:hs')?.is_direct,
    ).toBe(true)
    expect(env.refreshes()).toBe(0)

    release()

    await vi.waitFor(() => expect(env.refreshes()).toBe(1))
  })
})
