import { cleanup, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { LocationProvider } from 'preact-iso'
import { afterAll, afterEach, beforeAll, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import { createMembersStore } from '../stores/members'
import type { RoomDto } from '../stores/room-list'
import { TEST_BASE_URL, testServices } from '../test/services'
import { RoomInfoPanel } from './RoomInfoPanel'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const FIRST = '!first:hs'
const SECOND = '!second:hs'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
})
afterAll(() => server.close())

const room = (roomId: string, name: string): RoomDto =>
  ({
    account_id: ACCOUNT,
    account_user_id: '@me:example.org',
    room_id: roomId,
    name,
    last_activity_ts: 0,
    notification_count: 0,
    highlight_count: 0,
  }) as unknown as RoomDto

/**
 * `/info` answers per room; every other read is empty. `failSecond` makes the
 * second room's reads fail, which is what a room switch while offline looks
 * like.
 */
function handlers(options: { failSecond?: boolean } = {}) {
  const base = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId`
  const infoFor = (roomId: string) =>
    decodeURIComponent(roomId) === FIRST
      ? { encryption_algorithm: 'm.megolm.v1.aes-sha2', join_rule: 'invite' }
      : { encryption_algorithm: null, join_rule: 'public' }
  return [
    http.get(`${base}/info`, ({ params }) => {
      const roomId = String(params.roomId)
      if (options.failSecond === true && decodeURIComponent(roomId) !== FIRST) {
        return HttpResponse.json(
          { error: { code: 'unavailable', message: 'offline' } },
          { status: 503 },
        )
      }
      return HttpResponse.json({ data: infoFor(roomId) })
    }),
    http.get(`${base}/pinned`, () => HttpResponse.json({ data: [] })),
    http.get(`${base}/space/children`, () => HttpResponse.json({ data: [] })),
    http.get(`${base}/space/parents`, () => HttpResponse.json({ data: [] })),
    http.get(`${base}/upgrade`, () =>
      HttpResponse.json({
        data: { upgraded_from: null, tombstoned_to: null },
      }),
    ),
    http.get(`${base}/members`, () => HttpResponse.json({ data: [] })),
  ]
}

function renderPanel(roomId: string) {
  const services = testServices()
  const members = createMembersStore(services.api, ACCOUNT, roomId)
  const view = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <RoomInfoPanel
          accountId={ACCOUNT}
          roomId={roomId}
          room={room(roomId, roomId === FIRST ? 'First' : 'Second')}
          roomTitles={new Map()}
          members={members}
          onClose={() => {}}
        />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  const showRoom = (next: string) =>
    view.rerender(
      <ServicesContext.Provider value={services}>
        <LocationProvider>
          <RoomInfoPanel
            accountId={ACCOUNT}
            roomId={next}
            room={room(next, next === FIRST ? 'First' : 'Second')}
            roomTitles={new Map()}
            members={members}
            onClose={() => {}}
          />
        </LocationProvider>
      </ServicesContext.Provider>,
    )
  return { ...view, showRoom }
}

const detail = (label: string): string => {
  const terms = [...document.querySelectorAll('.detail-list dt')]
  const term = terms.find((node) => node.textContent === label)
  return term?.nextElementSibling?.textContent ?? ''
}

it('never shows the previous room’s state after a room switch', async () => {
  server.use(...handlers())
  const { showRoom } = renderPanel(FIRST)
  await waitFor(() => expect(detail('Encryption')).toBe('m.megolm.v1.aes-sha2'))

  // The panel is not remounted across room navigation, so a stale slice would
  // survive here — and encryption is exactly the field that must not lie.
  showRoom(SECOND)
  expect(detail('Encryption')).toBe('Loading…')
  await waitFor(() => expect(detail('Encryption')).toBe('Unencrypted'))
  expect(detail('Access')).toBe('public')
})

it('reports a failed room-state read instead of pending forever', async () => {
  server.use(...handlers({ failSecond: true }))
  const { showRoom, findByText } = renderPanel(FIRST)
  await waitFor(() => expect(detail('Encryption')).toBe('m.megolm.v1.aes-sha2'))

  showRoom(SECOND)
  await waitFor(() => expect(detail('Encryption')).toBe('Unavailable'))
  expect(await findByText('Could not load this section.')).toBeTruthy()
})

it('confirms before joining a relationship link, and reports a failed join', async () => {
  const base = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId`
  let joins = 0
  server.use(
    http.get(`${base}/space/children`, () =>
      HttpResponse.json({
        data: [{ room_id: '!child:hs', name: 'General', via: ['hs'] }],
      }),
    ),
    http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`, () => {
      joins += 1
      return HttpResponse.json(
        { error: { code: 'forbidden', message: 'no invite' } },
        { status: 403 },
      )
    }),
    ...handlers(),
  )
  const { findByRole, getByRole, findByText } = renderPanel(FIRST)

  // The label reads like navigation, but the room is not joined: opening it is
  // a membership change and must be confirmed.
  ;(await findByRole('button', { name: 'Child: General' })).click()
  expect(await findByText('Join this room?')).toBeTruthy()
  getByRole('button', { name: 'Cancel' }).click()
  expect(joins).toBe(0)
  ;(await findByRole('button', { name: 'Child: General' })).click()
  ;(await findByRole('button', { name: 'Join and open' })).click()

  await waitFor(() => expect(joins).toBe(1))
  expect(await findByText(/Could not join Child: General/)).toBeTruthy()
})

it('surfaces relationship errors rather than a permanent loading line', async () => {
  const base = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId`
  server.use(
    // Every relationship read fails, as when the server is unreachable. These
    // come first: msw resolves with the first matching handler.
    http.get(`${base}/space/children`, () =>
      HttpResponse.json(
        { error: { code: 'unavailable', message: 'offline' } },
        { status: 503 },
      ),
    ),
    http.get(`${base}/space/parents`, () =>
      HttpResponse.json(
        { error: { code: 'unavailable', message: 'offline' } },
        { status: 503 },
      ),
    ),
    http.get(`${base}/upgrade`, () =>
      HttpResponse.json(
        { error: { code: 'unavailable', message: 'offline' } },
        { status: 503 },
      ),
    ),
    ...handlers(),
  )
  const { queryAllByText, queryByText } = renderPanel(FIRST)

  // One per failed read: children, parents and upgrade.
  await waitFor(() =>
    expect(queryAllByText('Could not load this section.').length).toBe(3),
  )
  expect(queryByText('Loading room relationships…')).toBeNull()
})
