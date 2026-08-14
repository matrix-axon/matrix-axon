import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { LocationProvider, Route, Router } from 'preact-iso'
import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  it,
  vi,
} from 'vitest'
import { ServicesContext } from '../services'
import { TEST_BASE_URL, testServices } from '../test/services'
import { RoomPage } from './RoomPage'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM_A = '!a:hs'
const ROOM_B = '!b:hs'

const server = setupServer(
  http.get(`${TEST_BASE_URL}/v1/invites`, () =>
    HttpResponse.json({ data: [] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
    HttpResponse.json({
      data: [ROOM_A, ROOM_B].map((room_id) => ({
        account_id: ACCOUNT,
        account_user_id: '@me:hs',
        room_id,
        name: room_id,
        last_activity_ts: 0,
      })),
    }),
  ),
  http.get(
    `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/timeline`,
    () => HttpResponse.json({ data: { events: [], next_cursor: null } }),
  ),
  http.get(
    `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`,
    () => HttpResponse.json({ data: [] }),
  ),
  http.get(
    `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/members`,
    () => HttpResponse.json({ data: [] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () =>
    HttpResponse.json({ data: { namespace: 'drafts', entries: {} } }),
  ),
  http.put(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () =>
    HttpResponse.json({ data: { updated_at: '2026-06-01T12:00:00Z' } }),
  ),
  // Outbound read receipts + typing notices (ADR 0067/0068); no-op by default.
  http.post(`${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/read`, () =>
    HttpResponse.json({ data: {} }),
  ),
  http.put(`${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/typing`, () =>
    HttpResponse.json({ data: {} }),
  ),
)
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  vi.restoreAllMocks()
  window.history.replaceState(null, '', '/')
})
afterAll(() => server.close())

function renderAt(roomId: string) {
  window.history.replaceState(
    null,
    '',
    `/${ACCOUNT}/rooms/${encodeURIComponent(roomId)}`,
  )
  const services = testServices()
  const utils = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <Router>
          <Route path="/:accountId/rooms/:roomId" component={RoomPage} />
          <Route default component={RoomPage} />
        </Router>
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  return { services, ...utils }
}

/** Navigate the way a room-list click does; LocationProvider tracks popstate. */
async function goToRoom(
  utils: ReturnType<typeof renderAt>,
  roomId: string,
): Promise<HTMLTextAreaElement> {
  window.history.pushState(
    null,
    '',
    `/${ACCOUNT}/rooms/${encodeURIComponent(roomId)}`,
  )
  window.dispatchEvent(new PopStateEvent('popstate'))
  await waitFor(() => {
    expect(utils.getByText(roomId)).toBeTruthy()
  })
  return utils.getByLabelText(/^Message/) as HTMLTextAreaElement
}

describe('RoomPage drafts are scoped to their room (M-W6 step 5b)', () => {
  // RoomPage is not remounted when the route's roomId changes (no `key` on the
  // Route), so the composer must be room-scoped or an unsent draft follows you
  // into the next room — and Enter would send it there.
  it('does not carry an unsent draft from one room into the next', async () => {
    const utils = renderAt(ROOM_A)
    const textarea = await waitFor(
      () => utils.getByLabelText(/^Message/) as HTMLTextAreaElement,
    )
    fireEvent.input(textarea, { target: { value: 'secret for room A' } })

    expect((await goToRoom(utils, ROOM_B)).value).toBe('')
  })

  // ...but the draft is not lost: it round-trips through the device-state
  // cache, so returning to the room restores what you were typing.
  it('restores a room draft when you navigate back to it', async () => {
    const utils = renderAt(ROOM_A)
    const textarea = await waitFor(
      () => utils.getByLabelText(/^Message/) as HTMLTextAreaElement,
    )
    fireEvent.input(textarea, { target: { value: 'secret for room A' } })

    await goToRoom(utils, ROOM_B)
    expect((await goToRoom(utils, ROOM_A)).value).toBe('secret for room A')
  })

  it('keeps the resized composer height when changing rooms', async () => {
    const utils = renderAt(ROOM_A)
    const textarea = await waitFor(
      () => utils.getByLabelText(/^Message/) as HTMLTextAreaElement,
    )
    vi.spyOn(textarea, 'getBoundingClientRect').mockReturnValue({
      width: 320,
      height: 48,
      top: 0,
      left: 0,
      right: 320,
      bottom: 48,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    })

    fireEvent.keyDown(
      utils.getByRole('button', { name: 'Resize message input' }),
      { key: 'ArrowUp' },
    )
    expect(utils.services.settings.messageComposerHeight.value).toBe(72)

    expect((await goToRoom(utils, ROOM_B)).style.height).toBe('72px')
  })
})
