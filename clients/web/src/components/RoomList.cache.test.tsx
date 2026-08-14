import { cleanup, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { LocationProvider } from 'preact-iso'
import { afterAll, afterEach, beforeAll, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import { cacheNamespace, createMemoryCacheStore } from '../stores/cache-store'
import { TEST_BASE_URL, testServices } from '../test/services'
import { RoomList } from './RoomList'

/**
 * The room list painting from the durable cache (ADR 0085 phase 2), end to
 * end: a seeded cache, a `/v1/rooms` request held open, and the assertion that
 * rows are on screen *before* it answers — which is the entire user-visible
 * claim of this phase.
 */

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'

const OPS = {
  account_id: ACCOUNT,
  account_user_id: '@me:example.org',
  room_id: '!ops:hs',
  name: 'Ops',
  last_activity_ts: 100,
  notification_count: 0,
  highlight_count: 0,
}
const LOUNGE = { ...OPS, room_id: '!lounge:hs', name: 'Lounge' }

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
})
afterAll(() => server.close())

async function seededCache(rooms: unknown[]) {
  const cache = createMemoryCacheStore()
  const key = await cacheNamespace(TEST_BASE_URL, 'tok-test')
  await cache.write('rooms', key!, { version: 1, savedAt: 1, rooms })
  return cache
}

function handlers(rooms: unknown[], roomsResponse: () => Promise<Response>) {
  server.use(
    // The list narrows to rooms whose account is active, so this has to be
    // real: an empty accounts response filters every cached row away.
    http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
      HttpResponse.json({
        data: [
          {
            account_id: ACCOUNT,
            user_id: '@me:example.org',
            homeserver_url: 'https://matrix.example.org',
            state: 'active',
            device_id: 'AXONWEB',
            created_at: '2026-01-01T00:00:00Z',
            updated_at: '2026-01-01T00:00:00Z',
          },
        ],
      }),
    ),
    http.get(`${TEST_BASE_URL}/v1/rooms`, () => roomsResponse()),
    http.get(`${TEST_BASE_URL}/v1/invites`, () =>
      HttpResponse.json({ data: [] }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/space/children`,
      () => HttpResponse.json({ data: [] }),
    ),
    http.get(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () =>
      HttpResponse.json({
        data: { namespace: 'read_markers', entries: {} },
      }),
    ),
    http.put(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () =>
      HttpResponse.json({ data: { updated_at: '2026-07-20T12:00:00Z' } }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/members`,
      () => HttpResponse.json({ data: [] }),
    ),
  )
  return rooms
}

it('paints cached rooms while the request is still open, then settles', async () => {
  const cache = await seededCache([OPS, LOUNGE])
  let release: (() => void) | undefined
  const held = new Promise<void>((resolve) => (release = resolve))
  handlers([OPS], async () => {
    await held
    return HttpResponse.json({ data: [OPS] })
  })

  const services = testServices({ cache })
  const { findByText, queryByText, getByText } = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <RoomList />
      </LocationProvider>
    </ServicesContext.Provider>,
  )

  // Cached rows, with the quiet affordance — and never the loading message.
  expect(await findByText('Ops')).toBeTruthy()
  expect(getByText('Lounge')).toBeTruthy()
  expect(getByText('Updating…')).toBeTruthy()
  expect(queryByText('Loading rooms…')).toBeNull()

  release?.()

  // The refresh reconciles in place: the affordance goes, and the room the
  // server no longer lists goes with it.
  await waitFor(() => expect(queryByText('Updating…')).toBeNull())
  expect(queryByText('Lounge')).toBeNull()
  expect(getByText('Ops')).toBeTruthy()
})

it('still shows "Loading rooms…" with a cold cache', async () => {
  let release: (() => void) | undefined
  const held = new Promise<void>((resolve) => (release = resolve))
  handlers([OPS], async () => {
    await held
    return HttpResponse.json({ data: [OPS] })
  })

  const services = testServices()
  const { findByText, queryByText } = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <RoomList />
      </LocationProvider>
    </ServicesContext.Provider>,
  )

  expect(await findByText('Loading rooms…')).toBeTruthy()
  expect(queryByText('Updating…')).toBeNull()
  release?.()
  await waitFor(() => expect(queryByText('Loading rooms…')).toBeNull())
})
