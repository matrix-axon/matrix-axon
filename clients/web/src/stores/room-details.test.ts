import { signal } from '@preact/signals'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, expect, it, vi } from 'vitest'
import { createApiClient } from '../api/client'
import { ROOM_DETAILS_CHANGED, type LiveFrame } from '../api/frames'
import { homeserverDetailsDto, metadataDto } from '../test/room-details'
import type { LiveConnection } from './live-connection'
import {
  createRoomDetailsStore,
  emptyRoomDetails,
  nextLabelDeadline,
  roomDetailsKey,
} from './room-details'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const OTHER = '6b53f7f0-0000-4000-8000-000000000002'
const ROOM = '!room:hs'
const NEXT = '!next:hs'

const server = setupServer()
/** Every store a test built, disposed after it so none outlives its test. */
const disposers: Array<() => void> = []
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  for (const dispose of disposers.splice(0)) dispose()
  server.resetHandlers()
  vi.useRealTimers()
})
afterAll(() => server.close())

type Read = 'metadata' | 'info' | 'homeserver_details'

/**
 * Counts each read per room and lets a test hold responses open, so in-flight
 * coalescing and late answers can be driven deterministically.
 */
function harness() {
  const calls: Array<{ read: Read; roomId: string }> = []
  const held: Array<() => void> = []
  let holding = false
  let version = 0
  const count = (read: Read, roomId = ROOM) =>
    calls.filter((call) => call.read === read && call.roomId === roomId).length
  const respond = (read: Read, roomId: string) => {
    version += 1
    const answer = version
    calls.push({ read, roomId })
    const body = () =>
      HttpResponse.json({
        data:
          read === 'metadata'
            ? metadataDto()
            : read === 'info'
              ? {
                  member_counts: { joined: answer, invited: 0, observed_at: 0 },
                }
              : homeserverDetailsDto(),
      })
    if (!holding) return body()
    return new Promise<Response>((resolve) => held.push(() => resolve(body())))
  }
  const base = `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId`
  server.use(
    ...(['metadata', 'info', 'homeserver_details'] as const).map((read) =>
      http.get(`${base}/${read}`, ({ params }) =>
        respond(read, decodeURIComponent(String(params.roomId))),
      ),
    ),
  )
  const listeners = new Set<(frame: LiveFrame) => void>()
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
  const store = createRoomDetailsStore(api, live)
  disposers.push(() => store.dispose())
  const frame = (
    sources: readonly string[],
    roomId = ROOM,
    accountId = ACCOUNT,
  ) => {
    for (const listener of listeners) {
      listener({
        type: ROOM_DETAILS_CHANGED,
        accountId,
        payload: { room_id: roomId, sources },
      })
    }
  }
  return {
    store,
    calls,
    count,
    frame,
    reconnects,
    hold: () => {
      holding = true
    },
    release: () => {
      holding = false
      for (const resolve of held.splice(0)) resolve()
    },
    heldCount: () => held.length,
    listenerCount: () => listeners.size,
  }
}

const show = (h: ReturnType<typeof harness>, roomId = ROOM, joined = true) =>
  h.store.show({ accountId: ACCOUNT, roomId, joined })

it('reads metadata, member counts and homeserver details for a joined room', async () => {
  const h = harness()
  show(h)
  await vi.waitFor(() => {
    const state = h.store.state.value
    expect(state.key).toBe(roomDetailsKey(ACCOUNT, ROOM))
    expect(state.metadata.value).not.toBeNull()
    expect(state.info.value).not.toBeNull()
    expect(state.homeserverDetails.value).not.toBeNull()
  })
  expect(h.calls).toHaveLength(3)
})

it('never reads homeserver details for a room the account has not joined', async () => {
  const h = harness()
  show(h, ROOM, false)
  await vi.waitFor(() => expect(h.store.state.value.info.value).not.toBeNull())
  h.frame(['homeserver_details'])
  await vi.waitFor(() => expect(h.count('metadata')).toBe(1))
  expect(h.count('homeserver_details')).toBe(0)
  expect(h.store.state.value.homeserverDetails.requested).toBe(false)

  // The room list catches up: now it is joined, and the read becomes due.
  show(h, ROOM, true)
  await vi.waitFor(() => expect(h.count('homeserver_details')).toBe(1))
  expect(h.count('metadata')).toBe(1)
})

it('re-reads only the sources a frame names', async () => {
  const h = harness()
  show(h)
  await vi.waitFor(() => expect(h.calls).toHaveLength(3))

  h.frame(['member_counts'])
  await vi.waitFor(() => expect(h.count('info')).toBe(2))
  h.frame(['homeserver_details', 'space_hierarchy', 'discovery'])
  await vi.waitFor(() => expect(h.count('homeserver_details')).toBe(2))
  h.frame(['metadata'])
  await vi.waitFor(() => expect(h.count('metadata')).toBe(2))
  expect(h.calls).toHaveLength(6)
})

it('ignores frames for another account or another room', async () => {
  const h = harness()
  show(h)
  await vi.waitFor(() => expect(h.calls).toHaveLength(3))

  h.frame(['metadata', 'member_counts', 'homeserver_details'], NEXT)
  h.frame(['metadata', 'member_counts', 'homeserver_details'], ROOM, OTHER)
  h.frame(['metadata'])
  await vi.waitFor(() => expect(h.count('metadata')).toBe(2))
  expect(h.calls).toHaveLength(4)
})

it('folds frames that arrive mid-read into exactly one more read', async () => {
  const h = harness()
  show(h)
  await vi.waitFor(() => expect(h.calls).toHaveLength(3))

  h.hold()
  h.frame(['metadata'])
  await vi.waitFor(() => expect(h.heldCount()).toBe(1))
  // A burst while that read is in flight.
  h.frame(['metadata'])
  h.frame(['metadata'])
  h.frame(['metadata'])
  h.release()
  await vi.waitFor(() => expect(h.count('metadata')).toBe(3))
  // Give a wrongly-chained extra read the chance to show up.
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.count('metadata')).toBe(3)
})

it('re-reads everything it holds on a reconnect, once per reconnect', async () => {
  const h = harness()
  show(h)
  await vi.waitFor(() => expect(h.calls).toHaveLength(3))

  h.reconnects.value = 1
  await vi.waitFor(() => expect(h.calls).toHaveLength(6))
  expect(h.count('homeserver_details')).toBe(2)
})

it('does not re-read on open just because the socket reconnected long ago', async () => {
  const h = harness()
  h.reconnects.value = 3
  show(h)
  await vi.waitFor(() => expect(h.calls).toHaveLength(3))
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.calls).toHaveLength(3)
})

it('a reconnect with no room shown reads nothing', async () => {
  const h = harness()
  h.reconnects.value = 1
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.calls).toHaveLength(0)
})

it('drops the previous room’s answers on a switch, even late ones', async () => {
  const h = harness()
  h.hold()
  show(h)
  await vi.waitFor(() => expect(h.heldCount()).toBe(3))

  show(h, NEXT)
  // Nothing of the first room is visible under the new key.
  expect(h.store.state.value.key).toBe(roomDetailsKey(ACCOUNT, NEXT))
  expect(h.store.state.value.metadata.value).toBeNull()
  // The first room's aborted reads settle as failures; none of them may be
  // reported against the room now shown.
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.store.state.value.metadata.error).toBeNull()
  expect(h.store.state.value.info.error).toBeNull()
  h.release()
  await vi.waitFor(() => expect(h.store.state.value.info.value).not.toBeNull())
  expect(h.store.state.value.key).toBe(roomDetailsKey(ACCOUNT, NEXT))
  expect(h.store.state.value.info.value?.member_counts?.joined).toBeGreaterThan(
    3,
  )
  // A frame for the room the panel left re-reads nothing.
  const before = h.calls.length
  h.frame(['metadata'], ROOM)
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.calls).toHaveLength(before)
})

it('aborts the previous room’s in-flight reads on a switch', async () => {
  const h = harness()
  const aborted: string[] = []
  server.use(
    http.get(
      `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/metadata`,
      ({ request, params }) => {
        request.signal.addEventListener('abort', () =>
          aborted.push(decodeURIComponent(String(params.roomId))),
        )
        return new Promise<Response>(() => {})
      },
    ),
  )
  show(h)
  await new Promise((resolve) => setTimeout(resolve, 20))
  show(h, NEXT)
  await vi.waitFor(() => expect(aborted).toContain(ROOM))
  h.store.show(null)
})

it('closing then reopening the same room reads again', async () => {
  const h = harness()
  show(h)
  await vi.waitFor(() => expect(h.calls).toHaveLength(3))
  // Same room again while open: nothing new is due.
  show(h)
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.calls).toHaveLength(3)

  h.store.show(null)
  expect(h.store.state.value.key).toBeNull()
  show(h)
  await vi.waitFor(() => expect(h.calls).toHaveLength(6))
})

it('keeps the last answer through a failed re-read and reports the failure', async () => {
  const h = harness()
  show(h)
  await vi.waitFor(() =>
    expect(h.store.state.value.metadata.value).not.toBeNull(),
  )
  server.use(
    http.get(`${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/metadata`, () =>
      HttpResponse.json(
        { error: { code: 'unavailable', message: 'offline' } },
        { status: 503 },
      ),
    ),
  )
  h.frame(['metadata'])
  await vi.waitFor(() =>
    expect(h.store.state.value.metadata.error).toBe('offline'),
  )
  expect(h.store.state.value.metadata.value).not.toBeNull()
})

it('retry repeats only the reads that failed', async () => {
  const h = harness()
  server.use(
    http.get(
      `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/metadata`,
      () =>
        HttpResponse.json(
          { error: { code: 'unavailable', message: 'offline' } },
          { status: 503 },
        ),
      { once: true },
    ),
  )
  show(h)
  await vi.waitFor(() => {
    expect(h.store.state.value.metadata.error).toBe('offline')
    expect(h.store.state.value.info.value).not.toBeNull()
    expect(h.store.state.value.homeserverDetails.value).not.toBeNull()
  })
  h.store.retry()
  await vi.waitFor(() =>
    expect(h.store.state.value.metadata.value).not.toBeNull(),
  )
  expect(h.store.state.value.metadata.error).toBeNull()
  expect(h.count('metadata')).toBe(1)
  expect(h.count('info')).toBe(1)
  expect(h.count('homeserver_details')).toBe(1)
})

it('forgets homeserver details when the room is left with the panel open', async () => {
  const h = harness()
  show(h)
  await vi.waitFor(() =>
    expect(h.store.state.value.homeserverDetails.value).not.toBeNull(),
  )
  show(h, ROOM, false)
  expect(h.store.state.value.homeserverDetails).toEqual({
    value: null,
    error: null,
    requested: false,
  })
  // The rest of the room's answers stay, and its frames no longer read it.
  expect(h.store.state.value.metadata.value).not.toBeNull()
  h.frame(['homeserver_details'])
  await Promise.resolve()
  expect(h.count('homeserver_details')).toBe(1)
})

it('drops a homeserver answer that lands after the room was left', async () => {
  const h = harness()
  h.hold()
  show(h)
  await vi.waitFor(() => expect(h.heldCount()).toBe(3))
  show(h, ROOM, false)
  h.release()
  await vi.waitFor(() =>
    expect(h.store.state.value.metadata.value).not.toBeNull(),
  )
  await vi.waitFor(() => expect(h.store.state.value.info.value).not.toBeNull())
  expect(h.store.state.value.homeserverDetails.value).toBeNull()
})

it('dispose releases the frame listener, the reconnect effect and the visibility listener', async () => {
  const removed = vi.spyOn(document, 'removeEventListener')
  const h = harness()
  show(h)
  await vi.waitFor(() =>
    expect(h.store.state.value.homeserverDetails.value).not.toBeNull(),
  )
  expect(h.listenerCount()).toBe(1)
  h.store.dispose()
  expect(h.store.state.value.key).toBeNull()
  expect(h.listenerCount()).toBe(0)
  expect(removed).toHaveBeenCalledWith('visibilitychange', expect.any(Function))
  removed.mockRestore()
  // A store shown again after dispose (nothing should) proves the reconnect
  // effect is gone: a live one would re-read all three.
  show(h)
  await vi.waitFor(() => expect(h.calls).toHaveLength(6))
  h.reconnects.value += 1
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.calls).toHaveLength(6)
})

/**
 * Serve `/homeserver_details` with both details expiring at `expiresAt`, and
 * report how many times it was read.
 */
function expiringHomeserverDetails(expiresAt: number): () => number {
  let reads = 0
  const detail = {
    fetched_at: expiresAt - 15 * 60_000,
    expires_at: expiresAt,
    refresh: { state: 'idle' as const },
  }
  server.use(
    http.get(
      `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/homeserver_details`,
      () => {
        reads += 1
        return HttpResponse.json({
          data: homeserverDetailsDto({
            local_aliases: {
              status: 'available',
              aliases: ['#a:hs'],
              ...detail,
            },
            directory_visibility: {
              status: 'available',
              visibility: 'private',
              ...detail,
            },
          }),
        })
      },
    ),
  )
  return () => reads
}

it('moves the presentation clock past an expiry with no frame and no request', async () => {
  vi.useFakeTimers({
    toFake: ['setTimeout', 'clearTimeout', 'Date'],
    shouldAdvanceTime: true,
  })
  const h = harness()
  const expiresAt = Date.now() + 15_000
  const homeserverReads = expiringHomeserverDetails(expiresAt)
  show(h)
  await vi.waitFor(() =>
    expect(h.store.state.value.homeserverDetails.value).not.toBeNull(),
  )
  await vi.waitFor(() => expect(h.calls).toHaveLength(2))
  expect(homeserverReads()).toBe(1)
  expect(h.store.state.value.now).toBeLessThan(expiresAt)

  // The panel sits open and quiet: no frame, no reconnect.
  vi.advanceTimersByTime(15_001)
  expect(h.store.state.value.now).toBeGreaterThan(expiresAt)
  // Nothing was asked of the server to get there.
  expect(homeserverReads()).toBe(1)
  expect(h.calls).toHaveLength(2)
})

it('moves the clock when the page becomes visible after the device slept', async () => {
  const h = harness()
  show(h)
  await vi.waitFor(() => expect(h.calls).toHaveLength(3))
  const before = h.store.state.value.now
  // A suspended device does not run timers; wall time jumps meanwhile.
  vi.useFakeTimers({ toFake: ['Date'] })
  vi.setSystemTime(before + 60 * 60_000)
  document.dispatchEvent(new Event('visibilitychange'))
  expect(h.store.state.value.now).toBe(before + 60 * 60_000)
  expect(h.calls).toHaveLength(3)
})

it('arms nothing once every deadline has passed, so the clock never spins', () => {
  const state = {
    ...emptyRoomDetails('k'),
    homeserverDetails: {
      value: homeserverDetailsDto({
        local_aliases: {
          status: 'available',
          aliases: [],
          expires_at: 100,
          refresh: { state: 'idle', next_attempt_at: 50 },
        },
        directory_visibility: {
          status: 'available',
          visibility: 'public',
          expires_at: 300,
          refresh: { state: 'idle' },
        },
      }),
      error: null,
      requested: true,
    },
  }
  expect(nextLabelDeadline(state, 0)).toBe(50)
  expect(nextLabelDeadline(state, 60)).toBe(100)
  expect(nextLabelDeadline(state, 200)).toBe(300)
  expect(nextLabelDeadline(state, 301)).toBeNull()
})

it('includes a held absence check, whose backoff ending changes its label', () => {
  const state = {
    ...emptyRoomDetails('k'),
    metadata: {
      value: metadataDto({
        encryption: {
          status: 'unknown',
          invalid_fields: [],
          absence_confirmed_at: null,
          absence_check: { state: 'idle', next_attempt_at: 500 },
        },
      }),
      error: null,
      requested: true,
    },
  }
  expect(nextLabelDeadline(state, 0)).toBe(500)
})

it('stops the clock when the panel closes', async () => {
  vi.useFakeTimers({
    toFake: ['setTimeout', 'clearTimeout', 'Date'],
    shouldAdvanceTime: true,
  })
  const h = harness()
  const expiresAt = Date.now() + 15_000
  expiringHomeserverDetails(expiresAt)
  show(h)
  await vi.waitFor(() =>
    expect(h.store.state.value.homeserverDetails.value).not.toBeNull(),
  )
  h.store.show(null)
  const closed = h.store.state.value
  vi.advanceTimersByTime(15_001)
  expect(h.store.state.value).toBe(closed)
})

it('catches up within a minute after a laptop sleeps with the tab in view', async () => {
  vi.useFakeTimers({
    toFake: ['setTimeout', 'clearTimeout', 'Date'],
    shouldAdvanceTime: true,
  })
  const h = harness()
  const expiresAt = Date.now() + 10 * 60_000
  expiringHomeserverDetails(expiresAt)
  show(h)
  await vi.waitFor(() =>
    expect(h.store.state.value.homeserverDetails.value).not.toBeNull(),
  )
  const opened = h.store.state.value.now

  // Two hours asleep: the wall clock moves, the timers do not, and the page
  // was never hidden.
  vi.setSystemTime(opened + 2 * 60 * 60_000)
  vi.advanceTimersByTime(60_001)
  expect(h.store.state.value.now).toBeGreaterThan(expiresAt)
})

it('a recheck that finds nothing due leaves the state alone', async () => {
  vi.useFakeTimers({
    toFake: ['setTimeout', 'clearTimeout', 'Date'],
    shouldAdvanceTime: true,
  })
  const h = harness()
  expiringHomeserverDetails(Date.now() + 10 * 60_000)
  show(h)
  await vi.waitFor(() =>
    expect(h.store.state.value.homeserverDetails.value).not.toBeNull(),
  )
  await vi.waitFor(() => expect(h.calls).toHaveLength(2))
  const settled = h.store.state.value
  vi.advanceTimersByTime(3 * 60_000)
  expect(h.store.state.value).toBe(settled)
})
