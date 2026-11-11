import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from '@testing-library/preact'
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
import type { DeviceLinkPreview } from '../platform'
import type { LinkPreviewSetting } from '../stores/settings'
import type { EventDto } from '../stores/timeline'
import { TEST_BASE_URL, testServices } from '../test/services'
import { RoomPage } from './RoomPage'

/**
 * Composer link previews through the room and thread composers to the wire
 * (ADR 0132 § 6): what the server receives, not what a helper returns.
 */

vi.mock('emoji-picker-element', () => ({ Picker: class {} }))

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const OWN_USER = '@me:hs'
const UPLOAD_ID = '22222222-2222-4222-8222-222222222222'
const SEND_PATH = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`
const A = 'https://a.example.org/x'

function event(id: string, ts: number, overrides: object = {}): EventDto {
  return {
    account_id: ACCOUNT,
    event_id: id,
    room_id: ROOM,
    sender: '@alice:hs',
    origin_ts: ts,
    arrival_order: ts,
    type: 'm.room.message',
    body: `body of ${id}`,
    content: { msgtype: 'm.text', body: `body of ${id}` },
    redacted: false,
    edited: false,
    edit_count: 0,
    ...overrides,
  } as unknown as EventDto
}

const server = setupServer(
  http.get(
    `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/emoji-packs`,
    () =>
      HttpResponse.json(
        { error: { code: 'not_found', message: 'Older server' } },
        { status: 404 },
      ),
  ),
  http.get(`${TEST_BASE_URL}/v1/invites`, () =>
    HttpResponse.json({ data: [] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/accounts/:accountId/verify`, () =>
    HttpResponse.json({ data: [] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
    HttpResponse.json({
      data: [
        {
          account_id: ACCOUNT,
          account_user_id: OWN_USER,
          room_id: ROOM,
          name: 'Ops',
          last_activity_ts: 0,
        },
      ],
    }),
  ),
  http.get(
    `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/timeline`,
    () =>
      HttpResponse.json({
        data: { events: [event('$root', 100)], next_cursor: null },
      }),
  ),
  http.get(
    `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/threads`,
    () => HttpResponse.json({ data: [] }),
  ),
  http.get(
    `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/members`,
    () => HttpResponse.json({ data: [] }),
  ),
  http.get(
    `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/threads/:rootId/timeline`,
    () => HttpResponse.json({ data: { events: [], next_cursor: null } }),
  ),
  http.get(
    `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
    ({ params }) =>
      HttpResponse.json({
        data: event(params.eventId as string, 200, { sender: OWN_USER }),
      }),
  ),
  http.get(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () =>
    HttpResponse.json({ data: { namespace: 'drafts', entries: {} } }),
  ),
  http.put(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () =>
    HttpResponse.json({ data: { updated_at: '2026-06-01T12:00:00Z' } }),
  ),
  http.post(`${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/read`, () =>
    HttpResponse.json({ data: {} }),
  ),
  http.put(`${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/typing`, () =>
    HttpResponse.json({ data: {} }),
  ),
  http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/media/uploads`, () =>
    HttpResponse.json({ data: { upload_id: UPLOAD_ID } }),
  ),
)
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  window.history.replaceState(null, '', '/')
})
afterAll(() => server.close())

/** Capture each send body. */
function captureSends() {
  const sends: Record<string, unknown>[] = []
  server.use(
    http.post(SEND_PATH, async ({ request }) => {
      sends.push((await request.json()) as Record<string, unknown>)
      return HttpResponse.json({ data: { event_id: '$new' } })
    }),
  )
  return sends
}

function renderRoom({
  setting,
  linkPreview,
  thread = false,
}: {
  setting: LinkPreviewSetting
  linkPreview?: (
    url: string,
    signal?: AbortSignal,
  ) => Promise<DeviceLinkPreview>
  thread?: boolean
}) {
  window.history.replaceState(
    null,
    '',
    `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}${thread ? '?thread=%24root' : ''}`,
  )
  const services = testServices({
    now: () => 60_000,
    platform: linkPreview === undefined ? {} : { linkPreview },
  })
  services.settings.linkPreviews.value = setting
  render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <Router>
          <Route path="/:accountId/rooms/:roomId" component={RoomPage} />
          <Route default component={() => <div />} />
        </Router>
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  return services
}

const withImage = (url: string): DeviceLinkPreview => ({
  url,
  title: 'An article',
  image: {
    bytes: new Uint8Array([1, 2, 3]),
    mimeType: 'image/jpeg',
    width: 1,
    height: 1,
  },
  imageUnavailable: false,
})

async function paste(field: HTMLElement, value: string) {
  fireEvent.input(field, { target: { value }, inputType: 'insertFromPaste' })
}

describe('room composer previews on the wire', () => {
  it('sends a kept card with a freshly uploaded thumbnail', async () => {
    const sends = captureSends()
    const linkPreview = vi.fn(async (url: string) => withImage(url))
    renderRoom({ setting: 'device', linkPreview })
    const field = await screen.findByLabelText('Message Ops')
    await paste(field, `read ${A}`)
    await screen.findByText('An article')
    fireEvent.keyDown(field, { key: 'Enter' })
    await waitFor(() => expect(sends).toHaveLength(1))
    expect(sends[0].url_previews).toEqual([
      { matched_url: A, title: 'An article', image_upload_id: UPLOAD_ID },
    ])
  })

  it('sends the "no previews" marker for a link whose card is still loading', async () => {
    const sends = captureSends()
    const linkPreview = vi.fn(() => new Promise<DeviceLinkPreview>(() => {}))
    renderRoom({ setting: 'device', linkPreview })
    const field = await screen.findByLabelText('Message Ops')
    await paste(field, `read ${A}`)
    await screen.findByText('Loading preview…')
    fireEvent.keyDown(field, { key: 'Enter' })
    await waitFor(() => expect(sends).toHaveLength(1))
    expect(sends[0].url_previews).toEqual([])
  })

  it('leaves the field out for a message with no eligible link', async () => {
    const sends = captureSends()
    renderRoom({ setting: 'device', linkPreview: vi.fn() })
    const field = await screen.findByLabelText('Message Ops')
    fireEvent.input(field, {
      target: { value: 'plain http://a.example.org/x text' },
    })
    fireEvent.keyDown(field, { key: 'Enter' })
    await waitFor(() => expect(sends).toHaveLength(1))
    expect(sends[0]).not.toHaveProperty('url_previews')
  })

  it('leaves the field out when previews are off', async () => {
    const sends = captureSends()
    const linkPreview = vi.fn()
    renderRoom({ setting: 'off', linkPreview })
    const field = await screen.findByLabelText('Message Ops')
    await paste(field, `read ${A}`)
    fireEvent.keyDown(field, { key: 'Enter' })
    await waitFor(() => expect(sends).toHaveLength(1))
    expect(sends[0]).not.toHaveProperty('url_previews')
    expect(linkPreview).not.toHaveBeenCalled()
  })

  it('treats a stored device setting as off in a browser', async () => {
    const sends = captureSends()
    renderRoom({ setting: 'device' })
    const field = await screen.findByLabelText('Message Ops')
    await paste(field, `read ${A}`)
    expect(
      screen.queryByRole('list', { name: 'Link previews to send' }),
    ).toBeNull()
    fireEvent.keyDown(field, { key: 'Enter' })
    await waitFor(() => expect(sends).toHaveLength(1))
    expect(sends[0]).not.toHaveProperty('url_previews')
  })
})

describe('a send rejected for its previews (§ 6)', () => {
  it('says so on the failed echo, and Retry sends the marker', async () => {
    const sends: Record<string, unknown>[] = []
    server.use(
      http.post(SEND_PATH, async ({ request }) => {
        sends.push((await request.json()) as Record<string, unknown>)
        return sends.length === 1
          ? HttpResponse.json(
              { error: { code: 'preview_too_large', message: 'too large' } },
              { status: 413 },
            )
          : HttpResponse.json({ data: { event_id: '$new' } })
      }),
    )
    const linkPreview = vi.fn(async (url: string) => withImage(url))
    renderRoom({ setting: 'device', linkPreview })
    const field = await screen.findByLabelText('Message Ops')
    await paste(field, `read ${A}`)
    await screen.findByText('An article')
    fireEvent.keyDown(field, { key: 'Enter' })
    await screen.findByText(/the server refused its link previews/)
    fireEvent.click(screen.getByRole('button', { name: 'Retry' }))
    await waitFor(() => expect(sends).toHaveLength(2))
    expect(sends[1].url_previews).toEqual([])
    expect(sends[1].body).toBe(`read ${A}`)
  })
})

describe('thread composer previews on the wire', () => {
  it('sends a kept card with the thread relation', async () => {
    const sends = captureSends()
    const linkPreview = vi.fn(async (url: string) => withImage(url))
    renderRoom({ setting: 'device', linkPreview, thread: true })
    const panel = await screen.findByLabelText('Thread')
    const field = within(panel).getByRole('textbox')
    await paste(field, `read ${A}`)
    await within(panel).findByText('An article')
    fireEvent.keyDown(field, { key: 'Enter' })
    await waitFor(() => expect(sends).toHaveLength(1))
    expect(sends[0]).toMatchObject({
      thread_root: '$root',
      url_previews: [
        { matched_url: A, title: 'An article', image_upload_id: UPLOAD_ID },
      ],
    })
  })
})
