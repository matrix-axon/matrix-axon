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
import { TIMELINE_EVENT, type LiveFrame } from '../api/frames'
import type { LiveConnection } from './live-connection'
import type { RoomDto } from './room-list'
import type { RoomsStore } from './rooms'
import { createSpacesStore } from './spaces'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const SPACE = '!space:example.org'

const server = setupServer()
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
    { rooms, stale, error } as unknown as RoomsStore,
    live,
  )
  return {
    store,
    rooms,
    stale,
    error,
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
  expect(
    store.children.value
      .get(`${ACCOUNT}/${SPACE}`)
      ?.map((child) => child.room_id),
  ).not.toContain('!removed:hs')
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

it('treats subspace-only parents as empty in the shallow room picker', async () => {
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
