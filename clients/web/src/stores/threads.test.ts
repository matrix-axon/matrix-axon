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
import { createThreadsStore, threadRootId } from './threads'
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
