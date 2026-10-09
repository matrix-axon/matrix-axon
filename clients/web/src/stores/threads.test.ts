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
  createThreadsStore,
  ROOT_FETCH_CONCURRENCY,
  threadRootId,
} from './threads'
import type { EventDto } from './timeline'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function makeStore() {
  const api = createApiClient(
    {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
  return createThreadsStore(api, ACCOUNT, ROOM)
}

describe('createThreadsStore', () => {
  it('maps summaries by root and resolves root events', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`, () =>
        HttpResponse.json({
          data: [
            {
              root_event_id: '$root1',
              reply_count: 3,
              latest_reply_event_id: '$m3',
              latest_reply_ts: 300,
            },
            { root_event_id: '$root2', reply_count: 1 },
          ],
        }),
      ),
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        ({ params }) =>
          HttpResponse.json({
            data: {
              account_id: ACCOUNT,
              event_id: params.eventId,
              room_id: ROOM,
              sender: '@alice:hs',
              origin_ts: 1,
              arrival_order: 1,
              type: 'm.room.message',
              body: `root body ${params.eventId}`,
              redacted: false,
              edited: false,
              edit_count: 0,
            },
          }),
      ),
    )

    const store = makeStore()
    await store.refresh()

    expect(store.summaries.value.get('$root1')?.reply_count).toBe(3)
    expect(store.summaries.value.get('$root2')?.reply_count).toBe(1)
    await vi.waitFor(() => {
      expect(store.roots.value.get('$root1')?.body).toBe('root body $root1')
      expect(store.roots.value.get('$root2')).toBeDefined()
    })
    expect(store.loading.value).toBe(false)
  })

  /**
   * A room of many threads, with root fetches that stay open until released,
   * so the number in flight is something the test can read.
   */
  function manyThreads(count: number) {
    const asked: string[] = []
    const release: (() => void)[] = []
    let open = 0
    let peak = 0
    server.use(
      http.get(`${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`, () =>
        HttpResponse.json({
          data: Array.from({ length: count }, (_, i) => ({
            root_event_id: `$root${String(i)}`,
            reply_count: 1,
            // Oldest first on the wire, so the order asserted is the store's.
            latest_reply_ts: i,
          })),
        }),
      ),
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        async ({ params }) => {
          asked.push(String(params.eventId))
          open += 1
          peak = Math.max(peak, open)
          await new Promise<void>((resolve) => release.push(resolve))
          open -= 1
          return HttpResponse.json({
            data: { event_id: params.eventId, room_id: ROOM },
          })
        },
      ),
    )
    return {
      asked,
      peak: () => peak,
      /** Let every request now open finish. */
      releaseOpen: () => release.splice(0).forEach((done) => done()),
    }
  }

  /**
   * #662: a room with about 300 threads sent about 300 requests at once on
   * every open, which in the packaged app is as many TLS handshakes.
   */
  it('fetches roots a few at a time, most recently active thread first', async () => {
    const room = manyThreads(20)
    const store = makeStore()
    await store.refresh()

    await vi.waitFor(() =>
      expect(room.asked).toHaveLength(ROOT_FETCH_CONCURRENCY),
    )
    expect(room.asked.slice().sort()).toEqual(
      ['$root19', '$root18', '$root17', '$root16', '$root15', '$root14'].sort(),
    )

    while (store.roots.value.size < 20) {
      room.releaseOpen()
      await new Promise((resolve) => setTimeout(resolve, 5))
    }
    expect(room.asked).toHaveLength(20)
    expect(room.peak()).toBe(ROOT_FETCH_CONCURRENCY)
  })

  it('stops asking once the room is left, and asks again on a later refresh', async () => {
    const room = manyThreads(20)
    const store = makeStore()
    await store.refresh()
    await vi.waitFor(() =>
      expect(room.asked).toHaveLength(ROOT_FETCH_CONCURRENCY),
    )

    store.stop()
    room.releaseOpen()
    await vi.waitFor(() =>
      expect(store.roots.value.size).toBe(ROOT_FETCH_CONCURRENCY),
    )
    await new Promise((resolve) => setTimeout(resolve, 20))
    expect(room.asked).toHaveLength(ROOT_FETCH_CONCURRENCY)

    await store.refresh()
    await vi.waitFor(() =>
      expect(room.asked).toHaveLength(2 * ROOT_FETCH_CONCURRENCY),
    )
    expect(new Set(room.asked).size).toBe(2 * ROOT_FETCH_CONCURRENCY)
    while (store.roots.value.size < 20) {
      room.releaseOpen()
      await new Promise((resolve) => setTimeout(resolve, 5))
    }
  })

  /**
   * `stop()` used to empty only the queue that existed. A summaries response
   * still on its way refilled it, and all 20 roots were then fetched for a
   * room the reader had already left.
   */
  it('ignores a summary response that arrives after the room is left', async () => {
    const room = manyThreads(20)
    let answer!: () => void
    server.use(
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`,
        async () => {
          await new Promise<void>((resolve) => (answer = resolve))
          return HttpResponse.json({
            data: [{ root_event_id: '$late', reply_count: 1 }],
          })
        },
      ),
    )
    const store = makeStore()
    const refreshing = store.refresh()
    await vi.waitFor(() => expect(answer).toBeDefined())

    store.stop()
    answer()
    await refreshing
    await new Promise((resolve) => setTimeout(resolve, 20))

    expect(room.asked).toEqual([])
    expect(store.summaries.value.size).toBe(0)
    expect(store.loading.value).toBe(false)
  })

  it('surfaces list errors', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`, () =>
        HttpResponse.json(
          { error: { code: 'internal', message: 'boom' } },
          { status: 500 },
        ),
      ),
    )
    const store = makeStore()
    await store.refresh()
    expect(store.error.value).toBe('boom')
  })
})

describe('threadRootId', () => {
  const base: EventDto = {
    account_id: ACCOUNT,
    event_id: '$e',
    room_id: ROOM,
    sender: '@a:hs',
    origin_ts: 1,
    arrival_order: 1,
    type: 'm.room.message',
    redacted: false,
    edited: false,
    edit_count: 0,
  } as EventDto

  it('extracts m.thread roots and rejects other relations', () => {
    expect(
      threadRootId({
        ...base,
        relates_to: { rel_type: 'm.thread', event_id: '$root' },
      } as unknown as EventDto),
    ).toBe('$root')
    expect(threadRootId(base)).toBeNull()
    expect(
      threadRootId({
        ...base,
        relates_to: { 'm.in_reply_to': { event_id: '$t' } },
      } as unknown as EventDto),
    ).toBeNull()
  })
})
