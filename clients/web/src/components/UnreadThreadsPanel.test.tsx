import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { LocationProvider, Route, Router } from 'preact-iso'
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
import { ServicesContext } from '../services'
import { TEST_BASE_URL, testServices } from '../test/services'
import type { EventDto } from '../stores/timeline'
import { UnreadThreadsPanel } from './UnreadThreadsPanel'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const ROOM_PATH = `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`

const server = setupServer(
  http.get(`${TEST_BASE_URL}/v1/invites`, () =>
    HttpResponse.json({ data: [] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/accounts/:accountId/verify`, () =>
    HttpResponse.json({ data: [] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/rooms`, () => HttpResponse.json({ data: [] })),
  http.get(
    `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/threads`,
    () => HttpResponse.json({ data: [] }),
  ),
)
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  history.replaceState(null, '', '/')
})
afterAll(() => server.close())

function renderPanel(path: string, onClose = vi.fn()) {
  history.replaceState(null, '', path)
  const services = testServices()
  const utils = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <Router>
          <Route path="/" component={() => <p>Home</p>} />
          <Route
            path="/:accountId/rooms/:roomId"
            component={() => <p>Room</p>}
          />
        </Router>
        <UnreadThreadsPanel onClose={onClose} />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  return { services, onClose, ...utils }
}

describe('UnreadThreadsPanel', () => {
  it('keeps the global empty copy off a room route', () => {
    const { getByRole, getByText } = renderPanel('/')
    expect(getByRole('dialog', { name: 'Unread threads' })).toBeTruthy()
    expect(getByText('No unread threads.')).toBeTruthy()
  })

  it('does not toggle to room threads off a room route', async () => {
    const { services, findByRole, findByText, queryByRole } = renderPanel('/')
    services.threadUnread.recordLiveEvent(
      {
        account_id: ACCOUNT,
        room_id: ROOM,
        event_id: '$reply',
        sender: '@bob:hs',
        origin_ts: 200,
        arrival_order: 200,
        type: 'm.room.message',
        body: 'unread reply',
        relates_to: { rel_type: 'm.thread', event_id: '$unread-root' },
      } as unknown as EventDto,
      { roomTitle: 'Elsewhere', rootPreview: 'unread root' },
    )

    expect(await findByRole('dialog', { name: 'Unread threads' })).toBeTruthy()
    expect(await findByText('@bob:hs')).toBeTruthy()
    expect(await findByText('unread reply')).toBeTruthy()
    expect(queryByRole('button', { name: 'Unread threads' })).toBeNull()
  })

  it('lists the current room’s threads when nothing is unread', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/threads`,
        () =>
          HttpResponse.json({
            data: [
              {
                root_event_id: '$root',
                reply_count: 2,
                latest_reply_event_id: '$m2',
                latest_reply_ts: 300,
              },
            ],
          }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({
          data: {
            account_id: ACCOUNT,
            event_id: '$root',
            room_id: ROOM,
            sender: '@alice:hs',
            origin_ts: 100,
            arrival_order: 100,
            type: 'm.room.message',
            body: 'root preview',
            redacted: false,
            edited: false,
            edit_count: 0,
          },
        }),
      ),
    )
    const { findByRole, findByText, queryByText, onClose } =
      renderPanel(ROOM_PATH)

    expect(await findByRole('dialog', { name: 'Threads' })).toBeTruthy()
    expect(await findByText('root preview')).toBeTruthy()
    expect(await findByText('@alice:hs')).toBeTruthy()
    expect(await findByText('2 replies')).toBeTruthy()
    expect(queryByText('No unread threads.')).toBeNull()

    fireEvent.click(await findByText('root preview'))
    await waitFor(() => expect(window.location.search).toBe('?thread=%24root'))
    expect(onClose).toHaveBeenCalledOnce()
  })

  it('toggles from unread threads to the current room’s threads', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/threads`,
        () =>
          HttpResponse.json({
            data: [
              {
                root_event_id: '$room-root',
                reply_count: 1,
                latest_reply_ts: 50,
              },
            ],
          }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({
          data: {
            account_id: ACCOUNT,
            event_id: '$room-root',
            room_id: ROOM,
            sender: '@alice:hs',
            origin_ts: 40,
            arrival_order: 40,
            type: 'm.room.message',
            body: 'already read root',
            redacted: false,
            edited: false,
            edit_count: 0,
          },
        }),
      ),
    )
    const { services, findByRole, findByText, getByRole, queryByText } =
      renderPanel(ROOM_PATH)
    services.threadUnread.recordLiveEvent(
      {
        account_id: ACCOUNT,
        room_id: '!other:hs',
        event_id: '$reply',
        sender: '@bob:hs',
        origin_ts: 200,
        arrival_order: 200,
        type: 'm.room.message',
        body: 'unread reply',
        relates_to: { rel_type: 'm.thread', event_id: '$unread-root' },
      } as unknown as EventDto,
      { roomTitle: 'Elsewhere', rootPreview: 'unread root' },
    )

    expect(await findByRole('dialog', { name: 'Unread threads' })).toBeTruthy()
    expect(await findByText('unread reply')).toBeTruthy()
    const toggle = getByRole('button', { name: 'Unread threads' })
    expect(toggle.getAttribute('title')).toBe('Show threads in this room')

    fireEvent.click(toggle)

    expect(await findByRole('dialog', { name: 'Threads' })).toBeTruthy()
    expect(await findByText('already read root')).toBeTruthy()
    expect(await findByText('@alice:hs')).toBeTruthy()
    expect(queryByText('unread reply')).toBeNull()
    expect(getByRole('button', { name: 'Threads' }).getAttribute('title')).toBe(
      'Show unread threads',
    )

    fireEvent.click(getByRole('button', { name: 'Threads' }))
    expect(await findByText('unread reply')).toBeTruthy()
  })

  it('says there are no threads when the open room has none', async () => {
    const { findByRole, findByText } = renderPanel(ROOM_PATH)
    expect(await findByRole('dialog', { name: 'Threads' })).toBeTruthy()
    expect(await findByText('No threads.')).toBeTruthy()
  })
})
