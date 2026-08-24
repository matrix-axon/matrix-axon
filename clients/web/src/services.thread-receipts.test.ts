/**
 * `connectThreadReceipts` — learning that a thread was read on a client axon
 * does not run (#209).
 *
 * Element sends a thread-scoped `m.read` (MSC3771); it reaches us verbatim
 * through the ADR 0056 passthrough. Axon's own receipts are always unthreaded
 * (ADR 0096), so a `thread_id` on our own user's receipt can only have come
 * from elsewhere.
 */
import { computed, signal } from '@preact/signals'
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
import { createApiClient } from './api/client'
import { EPHEMERAL_PASSTHROUGH } from './api/frames'
import { connectThreadReceipts } from './services'
import type { AccountsStore } from './stores/accounts'
import { createDeviceStateStore } from './stores/device-state'
import { createLiveConnection } from './stores/live-connection'
import type { RoomsStore } from './stores/rooms'
import { createThreadUnreadStore } from './stores/thread-unread'
import type { EventDto } from './stores/timeline'
import { FakeWebSocket } from './test/fake-socket'
import { memoryStorage } from './test/memory-storage'

const ACCT = '11111111-1111-1111-1111-111111111111'
const ROOM = '!room:hs'
const ROOT = '$root'
const REPLY = '$reply'
const REPLY_TS = Date.UTC(2026, 7, 19, 12, 0, 0)
const ME = '@me:hs'
const BASE = 'http://axon.test'

const server = setupServer(
  http.get(`${BASE}/v1/accounts/:accountId/events/:eventId`, ({ params }) =>
    params.eventId === REPLY
      ? HttpResponse.json({
          data: {
            account_id: ACCT,
            event_id: REPLY,
            room_id: ROOM,
            sender: '@alice:hs',
            origin_ts: REPLY_TS,
            arrival_order: 42,
            type: 'm.room.message',
            body: 'a reply',
            content: {},
            redacted: false,
            edited: false,
            edit_count: 0,
            state_key: null,
            reactions: null,
          },
        })
      : new HttpResponse(null, { status: 404 }),
  ),
  http.put(`${BASE}/v1/devices/:deviceId/state/:namespace`, () =>
    HttpResponse.json({ data: { updated_at: '2026-08-19T12:00:00Z' } }),
  ),
  http.get(`${BASE}/v1/devices/:deviceId/state/:namespace`, () =>
    HttpResponse.json({
      data: { namespace: 'thread_read_markers', entries: {} },
    }),
  ),
)
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function harness(options: { accountsEmptyUntilRefresh?: boolean } = {}) {
  let socket: FakeWebSocket | undefined
  const live = createLiveConnection({
    socketFactory: () => {
      socket = new FakeWebSocket()
      return socket.asWebSocket()
    },
  })
  const api = createApiClient(
    {
      getToken: () => 't',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE,
  )
  const deviceState = createDeviceStateStore(api, live, memoryStorage())
  const threadUnread = createThreadUnreadStore()
  const known = signal(
    options.accountsEmptyUntilRefresh === true
      ? []
      : [{ account_id: ACCT, user_id: ME }],
  )
  let refreshes = 0
  const accounts = {
    accounts: computed(() => known.value),
    refresh: () => {
      refreshes += 1
      known.value = [{ account_id: ACCT, user_id: ME }]
      return Promise.resolve()
    },
  } as unknown as AccountsStore
  const rooms = {
    rooms: computed(() => signal([]).value),
  } as unknown as RoomsStore
  connectThreadReceipts(api, live, rooms, accounts, deviceState, threadUnread)
  live.start()
  socket!.emitOpen()

  // Something to clear: a live reply nobody has read.
  threadUnread.recordLiveEvent(
    {
      account_id: ACCT,
      event_id: REPLY,
      room_id: ROOM,
      sender: '@alice:hs',
      origin_ts: REPLY_TS,
      relates_to: { rel_type: 'm.thread', event_id: ROOT },
    } as unknown as EventDto,
    { roomTitle: 'Ops', ownUserId: ME },
  )
  return {
    deviceState,
    threadUnread,
    socket: () => socket!,
    refreshes: () => refreshes,
  }
}

const receiptFrame = (receipt: Record<string, unknown>, userId = ME) =>
  JSON.stringify({
    type: EPHEMERAL_PASSTHROUGH,
    account_id: ACCT,
    payload: {
      room_id: ROOM,
      event_type: 'm.receipt',
      content: { [REPLY]: { 'm.read': { [userId]: receipt } } },
    },
  })

describe('connectThreadReceipts', () => {
  it('records a thread read on another client as a durable marker', async () => {
    const { deviceState, threadUnread, socket } = harness()
    expect(threadUnread.isUnread(ACCT, ROOM, ROOT)).toBe(true)

    socket().emitMessage(receiptFrame({ ts: 1, thread_id: ROOT }))

    // The marker, not just the in-memory flag: receipts are live-only and never
    // replayed, so a session-scoped clear would come back on the next reload —
    // which is the complaint this exists to answer.
    await vi.waitFor(() =>
      expect(deviceState.threadReadMarker(ACCT, ROOM, ROOT)).toEqual({
        roomId: ROOM,
        rootEventId: ROOT,
        eventId: REPLY,
        originTs: REPLY_TS,
        // Carried from the looked-up event: a thread read in Element gives the
        // receipt path real arrival evidence, not just a display position.
        arrivalThrough: 42,
      }),
    )
    expect(threadUnread.isUnread(ACCT, ROOM, ROOT)).toBe(false)
  })

  it('retries after a lookup that never got an answer', async () => {
    const { deviceState, threadUnread, socket } = harness()
    let attempts = 0
    server.use(
      http.get(`${BASE}/v1/accounts/:accountId/events/:eventId`, () => {
        attempts += 1
        return attempts === 1
          ? HttpResponse.error()
          : HttpResponse.json({
              data: {
                account_id: ACCT,
                event_id: REPLY,
                room_id: ROOM,
                sender: '@alice:hs',
                origin_ts: REPLY_TS,
                arrival_order: 42,
                type: 'm.room.message',
                body: 'a reply',
                content: {},
                redacted: false,
                edited: false,
                edit_count: 0,
                state_key: null,
                reactions: null,
              },
            })
      }),
    )

    socket().emitMessage(receiptFrame({ ts: 1, thread_id: ROOT }))
    await vi.waitFor(() => expect(attempts).toBe(1))

    // A dropped connection is not a verdict about the event. Holding the dedup
    // key would discard this thread's read signal for the whole session, and
    // receipts are never replayed.
    socket().emitMessage(receiptFrame({ ts: 2, thread_id: ROOT }))
    await vi.waitFor(() =>
      expect(deviceState.threadReadMarker(ACCT, ROOM, ROOT)).not.toBeNull(),
    )
    expect(threadUnread.isUnread(ACCT, ROOM, ROOT)).toBe(false)
  })

  it('retries after a transient error status, but not after a 404', async () => {
    const { deviceState, socket } = harness()
    let attempts = 0
    server.use(
      http.get(`${BASE}/v1/accounts/:accountId/events/:eventId`, () => {
        attempts += 1
        // A 500 says nothing about the event; a 404 says it is gone.
        return attempts === 1
          ? new HttpResponse(null, { status: 500 })
          : new HttpResponse(null, { status: 404 })
      }),
    )

    socket().emitMessage(receiptFrame({ ts: 1, thread_id: ROOT }))
    await vi.waitFor(() => expect(attempts).toBe(1))

    // Retried, because the 500 released the dedup key.
    socket().emitMessage(receiptFrame({ ts: 2, thread_id: ROOT }))
    await vi.waitFor(() => expect(attempts).toBe(2))

    // Not retried after the 404, which is final.
    socket().emitMessage(receiptFrame({ ts: 3, thread_id: ROOT }))
    await new Promise((resolve) => setTimeout(resolve, 60))
    expect(attempts).toBe(2)
    expect(deviceState.threadReadMarker(ACCT, ROOM, ROOT)).toBeNull()
  })

  it('waits for hydration before writing, so it cannot regress a marker', async () => {
    let stateGets = 0
    server.use(
      http.get(
        `${BASE}/v1/devices/:deviceId/state/:namespace`,
        ({ params }) => {
          stateGets += 1
          return HttpResponse.json({
            data: {
              namespace: String(params.namespace),
              entries:
                params.namespace === 'thread_read_markers'
                  ? {
                      [`${encodeURIComponent(ROOM)}:${encodeURIComponent(ROOT)}`]:
                        {
                          value: {
                            room_id: ROOM,
                            root_event_id: ROOT,
                            event_id: '$later',
                            origin_ts: REPLY_TS + 10_000,
                            arrival_through: 99,
                          },
                        },
                    }
                  : {},
            },
          })
        },
      ),
    )
    const { deviceState, socket } = harness()

    // Nothing has opened a room, so this namespace has never been hydrated —
    // the state a live receipt meets right after login.
    expect(deviceState.threadReadMarker(ACCT, ROOM, ROOT)).toBeNull()
    socket().emitMessage(receiptFrame({ ts: 1, thread_id: ROOT }))

    await vi.waitFor(() => expect(stateGets).toBeGreaterThan(0))
    await vi.waitFor(() =>
      expect(
        deviceState.threadReadMarker(ACCT, ROOM, ROOT)?.eventId,
      ).toBeDefined(),
    )

    // The stored marker is further along than the receipt's event, so the
    // forward-only guard must keep it. Writing into an empty cache would have
    // skipped that guard and walked it backwards.
    expect(deviceState.threadReadMarker(ACCT, ROOM, ROOT)).toEqual({
      roomId: ROOM,
      rootEventId: ROOT,
      eventId: '$later',
      originTs: REPLY_TS + 10_000,
      arrivalThrough: 99,
    })
  })

  it('loads the accounts store rather than dropping a frame that beat it', async () => {
    const { deviceState, socket, refreshes } = harness({
      accountsEmptyUntilRefresh: true,
    })

    // A receipt can beat the stores on a reconnect. Returning here loses it for
    // good: receipts are live-only and nothing backfills them (#213).
    socket().emitMessage(receiptFrame({ ts: 1, thread_id: ROOT }))
    await vi.waitFor(() =>
      expect(deviceState.threadReadMarker(ACCT, ROOM, ROOT)).not.toBeNull(),
    )

    // A burst of frames must not become a burst of requests.
    socket().emitMessage(receiptFrame({ ts: 2, thread_id: '$other' }))
    await new Promise((resolve) => setTimeout(resolve, 40))
    expect(refreshes()).toBe(1)
  })

  it('ignores a main-timeline receipt', async () => {
    const { deviceState, threadUnread, socket } = harness()

    socket().emitMessage(receiptFrame({ ts: 1, thread_id: 'main' }))

    // `main` is a claim about the room stream, which has its own read position.
    // Asserting only on ROOT would pass either way — the bug this guards is a
    // marker written under the literal root `'main'`.
    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(deviceState.threadReadMarker(ACCT, ROOM, 'main')).toBeNull()
    expect(deviceState.threadReadMarker(ACCT, ROOM, ROOT)).toBeNull()
    expect(threadUnread.isUnread(ACCT, ROOM, ROOT)).toBe(true)
  })

  it('ignores an unthreaded receipt', async () => {
    const { deviceState, threadUnread, socket } = harness()

    // What axon's own receipts look like coming back around (ADR 0096): no
    // thread scope, so they say nothing about any thread.
    socket().emitMessage(receiptFrame({ ts: 1 }))

    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(deviceState.threadReadMarker(ACCT, ROOM, ROOT)).toBeNull()
    expect(threadUnread.isUnread(ACCT, ROOM, ROOT)).toBe(true)
  })

  it("ignores another user's threaded receipt", async () => {
    const { deviceState, threadUnread, socket } = harness()

    socket().emitMessage(
      receiptFrame({ ts: 1, thread_id: ROOT }, '@someone:hs'),
    )

    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(deviceState.threadReadMarker(ACCT, ROOM, ROOT)).toBeNull()
    expect(threadUnread.isUnread(ACCT, ROOM, ROOT)).toBe(true)
  })
})
