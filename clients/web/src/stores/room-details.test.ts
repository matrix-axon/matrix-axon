import { signal } from '@preact/signals'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, expect, it, vi } from 'vitest'
import { createApiClient } from '../api/client'
import { ROOM_DETAILS_CHANGED, type LiveFrame } from '../api/frames'
import { homeserverDetailsDto, metadataDto } from '../test/room-details'
import type { LiveConnection } from './live-connection'
import { createRoomDetailsStore, roomDetailsKey } from './room-details'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const OTHER = '6b53f7f0-0000-4000-8000-000000000002'
const ROOM = '!room:hs'
const NEXT = '!next:hs'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
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
