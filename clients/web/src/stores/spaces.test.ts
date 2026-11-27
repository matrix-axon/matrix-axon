import { signal } from '@preact/signals'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  it,
  vi,
} from 'vitest'
import { createApiClient } from '../api/client'
import {
  ROOM_DETAILS_CHANGED,
  TIMELINE_EVENT,
  type LiveFrame,
} from '../api/frames'
import type { LiveConnection } from './live-connection'
import type { RoomDto } from './room-list'
import type { RoomsStore } from './rooms'
import { adjacentSpace, createSpacesStore, spacePickerEntries } from './spaces'
import { spaceReadHandlers } from '../test/room-details'
import type { SpaceGraphLink } from './space-graph'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const SPACE = '!space:example.org'

const server = setupServer(...spaceReadHandlers(BASE_URL, 404))
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

const space = (roomId = SPACE): RoomDto =>
  ({
    account_id: ACCOUNT,
    room_id: roomId,
    room_type: 'm.space',
    name: roomId,
    last_activity_ts: 0,
  }) as unknown as RoomDto

const plainRoom = (roomId: string): RoomDto =>
  ({
    account_id: ACCOUNT,
    room_id: roomId,
    name: roomId,
    last_activity_ts: 0,
  }) as unknown as RoomDto

/** Only the members `createSpacesStore` touches; the rest of the store is not
 *  reachable from it. */
function harness() {
  const rooms = signal<RoomDto[]>([space()])
  const stale = signal(false)
  const error = signal<string | null>(null)
  const reconnects = signal(0)
  const counts = signal<ReadonlyMap<string, number>>(new Map())
  const listeners = new Set<(frame: LiveFrame) => void>()
  const api = createApiClient(
    {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
  const live = {
    reconnects,
    subscribe: (listener: (frame: LiveFrame) => void) => {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
  } as unknown as LiveConnection
  const store = createSpacesStore(
    api,
    {
      rooms,
      stale,
      error,
      unreadCount: (key: string) => counts.value.get(key) ?? 0,
    } as unknown as RoomsStore,
    live,
  )
  return {
    store,
    rooms,
    stale,
    error,
    counts,
    reconnects,
    emit: (frame: LiveFrame) => listeners.forEach((listen) => listen(frame)),
  }
}

const childrenUrl = `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(SPACE)}/space/children`

const settle = () => new Promise((resolve) => setTimeout(resolve, 0))

describe('createSpacesStore', () => {
  it('does not refetch children when the room list changes after a reconnect', async () => {
    let requests = 0
    server.use(
      http.get(childrenUrl, () => {
        requests += 1
        return HttpResponse.json({ data: [] })
      }),
    )
    const { rooms, reconnects } = harness()
    await settle()
    expect(requests).toBe(1)

    // One reconnect refetches every space exactly once…
    reconnects.value = 1
    await settle()
    expect(requests).toBe(2)

    // …and an ordinary room-list write (any incoming timeline event rewrites
    // this array) must not be mistaken for another one.
    rooms.value = [space(), plainRoom('!a:example.org')]
    await settle()
    rooms.value = [space(), plainRoom('!a:example.org'), plainRoom('!b:hs')]
    await settle()
    expect(requests).toBe(2)
  })

  it('re-runs a refresh that arrived while the request was in flight', async () => {
    let requests = 0
    let release: (() => void) | undefined
    server.use(
      http.get(childrenUrl, async () => {
        requests += 1
        if (requests === 1) {
          await new Promise<void>((resolve) => {
            release = resolve
          })
        }
        return HttpResponse.json({ data: [] })
      }),
    )
    const { emit } = harness()
    await settle()
    expect(requests).toBe(1)

    // An `m.space.child` frame lands while the initial fetch is still open: the
    // in-flight request cannot reflect it, so it has to be re-queued.
    emit({
      type: TIMELINE_EVENT,
      accountId: ACCOUNT,
      payload: {
        account_id: ACCOUNT,
        room_id: SPACE,
        type: 'm.space.child',
      },
    } as unknown as LiveFrame)
    release?.()
    await settle()
    await settle()
    expect(requests).toBe(2)
  })

  it('reports the server message when a space fails to load', async () => {
    server.use(
      http.get(childrenUrl, () =>
        HttpResponse.json(
          { error: { code: 'forbidden', message: 'not in room' } },
          { status: 403 },
        ),
      ),
    )
    const { store } = harness()
    await settle()
    expect(store.errors.value.get(`${ACCOUNT}/${SPACE}`)).toBe(
      'Could not load space: not in room',
    )
    expect(store.loading.value.has(`${ACCOUNT}/${SPACE}`)).toBe(false)
  })
})

it('hides successful empty membership and returns a selected empty space to All', async () => {
  server.use(http.get(childrenUrl, () => HttpResponse.json({ data: [] })))
  const { store } = harness()
  expect(store.visible.value.map((room) => room.room_id)).toEqual([SPACE])
  store.selected.value = `${ACCOUNT}/${SPACE}`
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  expect(store.visible.value).toEqual([])
  expect(store.selected.value).toBeNull()
})

it('ignores removed relationships, unjoined children, and other accounts when testing emptiness', async () => {
  server.use(
    http.get(childrenUrl, () =>
      HttpResponse.json({
        data: [
          { room_id: '!removed:hs', via: [], suggested: false },
          { room_id: '!foreign:hs', via: ['hs'], suggested: false },
          { room_id: '!absent:hs', via: ['hs'], suggested: false },
          { room_id: SPACE, via: ['hs'], suggested: false },
        ],
      }),
    ),
  )
  const { store, rooms } = harness()
  rooms.value = [
    space(),
    plainRoom('!removed:hs'),
    { ...plainRoom('!foreign:hs'), account_id: 'other-account' },
  ]
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  expect(store.parents.value.has(`${ACCOUNT}/!removed:hs`)).toBe(false)
  expect(store.visible.value).toEqual([])
  rooms.value = [space(), plainRoom('!foreign:hs')]
  expect(store.visible.value.map((room) => room.room_id)).toEqual([SPACE])
})

it('keeps confirmed empty membership hidden during refetch but reveals failures', async () => {
  let fail = false
  let release: (() => void) | undefined
  server.use(
    http.get(childrenUrl, async () => {
      if (fail) {
        await new Promise<void>((resolve) => {
          release = resolve
        })
        return HttpResponse.json(
          { error: { code: 'offline', message: 'unavailable' } },
          { status: 503 },
        )
      }
      return HttpResponse.json({ data: [] })
    }),
  )
  const { store } = harness()
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  expect(store.visible.value).toEqual([])
  fail = true
  store.refresh(space())
  expect(store.visible.value).toEqual([])
  await vi.waitFor(() => expect(release).toBeDefined())
  expect(store.visible.value).toEqual([])
  release?.()
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  expect(store.visible.value.map((room) => room.room_id)).toEqual([SPACE])
  store.selected.value = `${ACCOUNT}/${SPACE}`
  expect(store.selected.value).toBe(`${ACCOUNT}/${SPACE}`)
})

it('refetches hidden roots after child updates and restores them when joined children appear', async () => {
  let data: Array<{ room_id: string; via: string[]; suggested: boolean }> = []
  server.use(http.get(childrenUrl, () => HttpResponse.json({ data })))
  const { store, rooms, emit } = harness()
  rooms.value = [space(), plainRoom('!child:hs')]
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  expect(store.visible.value).toEqual([])
  data = [{ room_id: '!child:hs', via: ['hs'], suggested: false }]
  emit({
    type: TIMELINE_EVENT,
    accountId: ACCOUNT,
    payload: {
      account_id: ACCOUNT,
      room_id: SPACE,
      type: 'm.space.child',
    },
  } as unknown as LiveFrame)
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  expect(store.visible.value.map((room) => room.room_id)).toEqual([SPACE])
})

it('does not infer empty membership from a stale or failed room catalog', async () => {
  server.use(
    http.get(childrenUrl, () =>
      HttpResponse.json({
        data: [{ room_id: '!child:hs', via: ['hs'], suggested: false }],
      }),
    ),
  )
  const { store, rooms, stale, error } = harness()
  stale.value = true
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  expect(store.visible.value.map((room) => room.room_id)).toEqual([SPACE])
  stale.value = false
  expect(store.visible.value).toEqual([])
  error.value = 'Could not load rooms'
  expect(store.visible.value.map((room) => room.room_id)).toEqual([SPACE])
  rooms.value = [space(), plainRoom('!child:hs')]
  error.value = null
  expect(store.visible.value.map((room) => room.room_id)).toEqual([SPACE])
})

it('keeps the shallow picker when the server has no graph route', async () => {
  server.use(
    http.get(
      `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/space/children`,
      ({ params }) =>
        HttpResponse.json({
          data:
            String(params.roomId) === SPACE
              ? [{ room_id: '!nested:hs', via: ['hs'], suggested: false }]
              : [{ room_id: '!child:hs', via: ['hs'], suggested: false }],
        }),
    ),
  )
  const { store, rooms } = harness()
  rooms.value = [space(), space('!nested:hs'), plainRoom('!child:hs')]
  store.selected.value = `${ACCOUNT}/${SPACE}`
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  expect(store.visible.value.map((room) => room.room_id)).toEqual([
    '!nested:hs',
  ])
  expect(store.selected.value).toBeNull()
})

const OTHER = '6b53f7f0-0000-4000-8000-000000000002'
const inOther = (roomId: string, roomType?: string): RoomDto =>
  ({
    account_id: OTHER,
    room_id: roomId,
    room_type: roomType,
    name: roomId,
    last_activity_ts: 0,
  }) as unknown as RoomDto

it('hides spaces the filtered account has no joined rooms in', async () => {
  server.use(
    http.get(
      `${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/space/children`,
      () =>
        HttpResponse.json({
          data: [{ room_id: '!child:hs', via: ['hs'], suggested: false }],
        }),
    ),
  )
  const { store, rooms } = harness()
  // The same space and child exist for both accounts, but only OTHER's space
  // has the child joined; ACCOUNT is in the space without that room.
  rooms.value = [space(), inOther(SPACE, 'm.space'), inOther('!child:hs')]
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  expect(store.visible.value.map((room) => room.account_id)).toEqual([OTHER])
  store.accountFilter.value = ACCOUNT
  expect(store.visible.value).toEqual([])
  store.accountFilter.value = OTHER
  expect(store.visible.value.map((room) => room.account_id)).toEqual([OTHER])
  store.accountFilter.value = 'all'
  expect(store.visible.value.map((room) => room.account_id)).toEqual([OTHER])
})

it('clears the selected space when the account filter hides it', async () => {
  server.use(
    http.get(childrenUrl, () =>
      HttpResponse.json({
        data: [{ room_id: '!child:hs', via: ['hs'], suggested: false }],
      }),
    ),
  )
  const { store, rooms } = harness()
  rooms.value = [space(), plainRoom('!child:hs')]
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  store.selected.value = `${ACCOUNT}/${SPACE}`
  store.accountFilter.value = OTHER
  expect(store.selected.value).toBeNull()
})

it('sums unread over joined direct children only', async () => {
  server.use(
    http.get(childrenUrl, () =>
      HttpResponse.json({
        data: [
          { room_id: '!a:hs', via: ['hs'], suggested: false },
          { room_id: '!b:hs', via: ['hs'], suggested: false },
          { room_id: '!unjoined:hs', via: ['hs'], suggested: false },
        ],
      }),
    ),
  )
  const { store, rooms, counts } = harness()
  rooms.value = [space(), plainRoom('!a:hs'), plainRoom('!b:hs')]
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  const key = `${ACCOUNT}/${SPACE}`
  expect(store.unread.value.has(key)).toBe(false)
  counts.value = new Map([
    [`${ACCOUNT}/!a:hs`, 3],
    [`${ACCOUNT}/!b:hs`, 4],
    [`${ACCOUNT}/!unjoined:hs`, 50],
    [`${ACCOUNT}/!elsewhere:hs`, 9],
  ])
  expect(store.unread.value.get(key)).toBe(7)
  counts.value = new Map()
  expect(store.unread.value.has(key)).toBe(false)
})

it('keeps the account filter when the room catalog is stale or failed', async () => {
  server.use(
    http.get(
      `${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/space/children`,
      () => HttpResponse.json({ data: [] }),
    ),
  )
  const { store, rooms, stale, error } = harness()
  rooms.value = [space(), inOther(SPACE, 'm.space')]
  store.accountFilter.value = OTHER
  stale.value = true
  expect(store.visible.value.map((room) => room.account_id)).toEqual([OTHER])
  stale.value = false
  error.value = 'Could not load rooms'
  expect(store.visible.value.map((room) => room.account_id)).toEqual([OTHER])
})

const graphUrl = `${BASE_URL}/v1/accounts/:accountId/spaces/graph`
const discoveredUrl = `${BASE_URL}/v1/accounts/:accountId/spaces/discovered`
const key = (roomId: string) => `${ACCOUNT}/${roomId}`
const link = (
  parent: string,
  child: string,
  order?: string,
  origin_ts = 0,
): SpaceGraphLink => ({
  parent_room_id: parent,
  child_room_id: child,
  source: 'child',
  canonical: false,
  order,
  origin_ts,
})

function change(source: 'space_graph' | 'space_browse', accountId = ACCOUNT) {
  return {
    type: ROOM_DETAILS_CHANGED,
    accountId,
    payload: { room_id: '!different-room:hs', sources: [source] },
  }
}

it('expands joined branches in Matrix order while keeping saved root order', async () => {
  const early = '!early:hs'
  const later = '!later:hs'
  const otherRoot = '!other-root:hs'
  const leaf = '!leaf:hs'
  const oldest = '!oldest:hs'
  const tieFirst = '!earlier-id:hs'
  server.use(
    http.get(graphUrl, () =>
      HttpResponse.json({
        data: {
          links: [
            link(SPACE, later),
            link(SPACE, tieFirst, undefined, -2),
            link(SPACE, oldest, undefined, -2),
            link(SPACE, early, 'A'),
            link(later, leaf),
            {
              ...link(otherRoot, SPACE),
              source: 'parent_claim',
              canonical: true,
            },
            link(early, leaf),
            link(otherRoot, early),
            link(SPACE, early, 'A'),
          ],
        },
      }),
    ),
  )
  const { store, rooms, counts } = harness()
  rooms.value = [
    space(),
    space(early),
    space(later),
    space(oldest),
    space(tieFirst),
    space(otherRoot),
    plainRoom(leaf),
  ]
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  const entries = () => spacePickerEntries(store, [key(otherRoot)], new Map())
  expect(entries().map(({ space }) => space.room_id)).toEqual([
    otherRoot,
    SPACE,
  ])
  for (const { path } of entries()) store.setExpanded(path, true)
  expect(entries().map(({ space }) => space.room_id)).toEqual([
    otherRoot,
    early,
    SPACE,
    early,
    tieFirst,
    oldest,
    later,
  ])

  store.selected.value = key(SPACE)
  expect(store.selectedRooms.value).toEqual(new Set([key(leaf)]))
  counts.value = new Map([
    [key(leaf), 3],
    [key(early), 90],
  ])
  expect(store.unread.value).toEqual(
    new Map([SPACE, early, later, otherRoot].map((id) => [key(id), 3])),
  )
  store.selected.value = key(early)
  expect(store.selectedRooms.value).toEqual(new Set([key(leaf)]))
})

it('cuts picker cycles and keeps every space reachable and selectable', async () => {
  const second = '!second:hs'
  const leaf = '!leaf:hs'
  server.use(
    http.get(graphUrl, () =>
      HttpResponse.json({
        data: {
          links: [link(SPACE, second), link(second, SPACE), link(SPACE, leaf)],
        },
      }),
    ),
  )
  const { store, rooms, counts } = harness()
  rooms.value = [space(), space(second), plainRoom(leaf)]
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  const entries = () => spacePickerEntries(store, [], new Map())
  expect(entries().map(({ space }) => space.room_id)).toEqual([second])
  const rootPath = entries()[0].path
  store.setExpanded(rootPath, true)
  expect(entries().map(({ space }) => space.room_id)).toEqual([second, SPACE])
  expect(entries()[1].expandable).toBe(false)
  store.selected.value = key(second)
  expect(store.selectedRooms.value).toEqual(new Set([key(leaf)]))
  counts.value = new Map([[key(leaf), 4]])
  expect(store.unread.value.get(key(SPACE))).toBe(4)
  expect(store.unread.value.get(key(second))).toBe(4)
  store.resetSession()
  expect(store.selected.value).toBeNull()
  expect(store.expanded.value).toEqual(new Set([rootPath]))
})

it('reloads account graph pages after invalidation during pagination and removes dropped links', async () => {
  const child = '!child:hs'
  const requests: string[] = []
  let release: (() => void) | undefined
  server.use(
    http.get(graphUrl, async ({ request }) => {
      const cursor = new URL(request.url).searchParams.get('cursor') ?? ''
      requests.push(cursor)
      if (requests.length === 1) {
        return HttpResponse.json({ data: { links: [], next_cursor: 'second' } })
      }
      if (cursor === 'second') {
        await new Promise<void>((resolve) => {
          release = resolve
        })
        return HttpResponse.json({ data: { links: [link(SPACE, child)] } })
      }
      return HttpResponse.json({ data: { links: [] } })
    }),
  )
  const { store, rooms, emit, reconnects } = harness()
  rooms.value = [space(), plainRoom(child)]
  await vi.waitFor(() => expect(release).toBeDefined())
  emit(change('space_graph', OTHER))
  expect(requests).toEqual(['', 'second'])
  emit(change('space_graph'))
  release?.()
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  expect(requests).toEqual(['', 'second', ''])
  expect(store.visible.value).toEqual([])
  reconnects.value += 1
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  expect(requests).toEqual(['', 'second', '', ''])
})

it('uses cached discovered memberships through ancestors and expires them locally', async () => {
  const nested = '!nested:hs'
  const joinedSpace = '!through:hs'
  const leaf = '!leaf:hs'
  const remoteOnly = '!remote-only:hs'
  const end = Date.now() + 60_000
  const discover = vi.fn(({ request }: { request: Request }) =>
    new URL(request.url).searchParams.has('cursor')
      ? HttpResponse.json({
          data: {
            memberships: [
              { space_id: nested, room_id: joinedSpace, expires_at: end },
            ],
          },
        })
      : HttpResponse.json({
          data: {
            memberships: [
              { space_id: nested, room_id: remoteOnly, expires_at: end },
            ],
            next_cursor: 'next',
          },
        }),
  )
  server.use(
    http.get(graphUrl, () =>
      HttpResponse.json({
        data: { links: [link(SPACE, nested), link(joinedSpace, leaf)] },
      }),
    ),
    http.get(discoveredUrl, discover),
  )
  const { store, rooms, counts } = harness()
  rooms.value = [space(), space(nested), space(joinedSpace), plainRoom(leaf)]
  vi.useFakeTimers({ toFake: ['Date', 'setTimeout', 'clearTimeout'] })
  try {
    await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
    store.selected.value = key(SPACE)
    expect(store.selectedRooms.value).toEqual(new Set([key(leaf)]))
    expect(discover).toHaveBeenCalledTimes(2)
    counts.value = new Map([[key(leaf), 2]])
    expect(store.unread.value.get(key(SPACE))).toBe(2)
    expect(store.unread.value.get(key(nested))).toBe(2)
    expect(store.nested.value.has(key(joinedSpace))).toBe(false)
    vi.setSystemTime(end - 1)
    await vi.advanceTimersByTimeAsync(60_001)
    expect(store.selectedRooms.value).toEqual(new Set())
    expect(store.unread.value.has(key(SPACE))).toBe(false)
    expect(discover).toHaveBeenCalledTimes(2)
  } finally {
    vi.useRealTimers()
  }
})

it('reconciles cached memberships without re-reading the graph on browse invalidation', async () => {
  const child = '!child:hs'
  let present = true
  const graph = vi.fn(() => HttpResponse.json({ data: { links: [] } }))
  server.use(
    http.get(graphUrl, graph),
    http.get(discoveredUrl, () =>
      HttpResponse.json({
        data: {
          memberships: present
            ? [
                {
                  space_id: SPACE,
                  room_id: child,
                  expires_at: Date.now() + 60_000,
                },
              ]
            : [],
        },
      }),
    ),
  )
  const { store, rooms, emit } = harness()
  rooms.value = [space(), plainRoom(child)]
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  store.selected.value = key(SPACE)
  expect(store.selectedRooms.value).toEqual(new Set([key(child)]))
  present = false
  emit(change('space_browse'))
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  expect(store.visible.value).toEqual([])
  expect(store.selected.value).toBeNull()
  expect(graph).toHaveBeenCalledTimes(1)
})

it('loads legacy spaces independently and retries only the affected space', async () => {
  const slow = '!slow:hs'
  const child = '!leaf:hs'
  const requests: string[] = []
  let release: (() => void) | undefined
  server.use(
    http.get(
      `${BASE_URL}/v1/accounts/:accountId/rooms/:roomId/space/children`,
      async ({ params }) => {
        const roomId = String(params.roomId)
        requests.push(roomId)
        if (roomId === slow) {
          await new Promise<void>((resolve) => {
            release = resolve
          })
          return HttpResponse.json({ data: [] })
        }
        return HttpResponse.json({
          data: [{ room_id: child, via: ['hs'], suggested: false }],
        })
      },
    ),
  )
  const { store, rooms, emit } = harness()
  rooms.value = [space(), space(slow), plainRoom(child)]
  store.selected.value = key(SPACE)
  try {
    await vi.waitFor(() => expect(release).toBeDefined())
    await vi.waitFor(() =>
      expect(store.selectedRooms.value).toEqual(new Set([key(child)])),
    )
    expect(store.loading.value.size).toBe(1)
    emit({
      type: TIMELINE_EVENT,
      accountId: ACCOUNT,
      payload: { account_id: ACCOUNT, room_id: SPACE, type: 'm.space.child' },
    })
    await vi.waitFor(() =>
      expect(requests.filter((id) => id === SPACE)).toHaveLength(2),
    )
    store.refresh(space())
    await vi.waitFor(() =>
      expect(requests.filter((id) => id === SPACE)).toHaveLength(3),
    )
    expect(requests.filter((id) => id === slow)).toHaveLength(1)
  } finally {
    release?.()
    await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  }
})

it('keeps the joined graph usable when the optional discovery read fails', async () => {
  const empty = '!empty:hs'
  const child = '!leaf:hs'
  let release: (() => void) | undefined
  server.use(
    http.get(graphUrl, () =>
      HttpResponse.json({ data: { links: [link(SPACE, child)] } }),
    ),
    http.get(discoveredUrl, async () => {
      await new Promise<void>((resolve) => {
        release = resolve
      })
      return new HttpResponse(null, { status: 503 })
    }),
  )
  const { store, rooms } = harness()
  rooms.value = [space(), space(empty), plainRoom(child)]
  store.selected.value = key(SPACE)
  try {
    await vi.waitFor(() => expect(release).toBeDefined())
    await vi.waitFor(() =>
      expect(store.selectedRooms.value).toEqual(new Set([key(child)])),
    )
    expect(store.parents.value.get(key(child))?.get(key(SPACE))).toBeNull()
  } finally {
    release?.()
    await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  }
  expect(store.errors.value.size).toBe(0)
  expect(store.visible.value.map((room) => room.room_id)).toEqual([SPACE])
})

it('cycles from the position of a selected child inside a collapsed branch', async () => {
  const before = '!before:hs'
  const after = '!after:hs'
  const nested = '!nested:hs'
  const leaf = '!leaf:hs'
  server.use(
    http.get(graphUrl, () =>
      HttpResponse.json({
        data: {
          links: [link(before, leaf), link(SPACE, nested), link(after, leaf)],
        },
      }),
    ),
  )
  const { store, rooms } = harness()
  rooms.value = [
    space(before),
    space(),
    space(nested),
    space(after),
    plainRoom(leaf),
  ]
  await vi.waitFor(() => expect(store.loading.value.size).toBe(0))
  const order = [key(before), key(SPACE), key(after)]
  const titles = new Map<string, string>()
  store.selected.value = key(nested)
  expect(adjacentSpace(store, order, titles, -1)).toBe(key(SPACE))
  expect(adjacentSpace(store, order, titles, 1)).toBe(key(after))
})
