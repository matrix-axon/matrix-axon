import {
  cleanup,
  fireEvent,
  render,
  waitFor,
  within,
} from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import type { JSX } from 'preact'
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
import { SINGLE_PANE_QUERY } from '../layout'
import type { EventDto } from '../stores/timeline'
import { TEST_BASE_URL, testServices } from '../test/services'
import { RoomPage } from './RoomPage'

// The real picker ships a heavy IndexedDB-backed data layer jsdom can't run, so
// the *module* is stubbed — the component keeps its one real mount path
// (dynamic import → `new Picker({ dataSource })` → `emoji-click`), which is the
// part these tests are here to exercise.
const pickerProps = vi.hoisted(() => [] as unknown[])
vi.mock('emoji-picker-element', () => {
  class Picker extends HTMLElement {
    constructor(props?: unknown) {
      super()
      pickerProps.push(props)
      const root = this.attachShadow({ mode: 'open' })
      const search = document.createElement('input')
      search.type = 'search'
      search.setAttribute('aria-label', 'Search emojis')
      root.append(search)

      const nav = document.createElement('div')
      root.append(nav)
      for (const label of ['Smileys', 'People']) {
        const button = document.createElement('button')
        button.className = 'nav-button'
        button.type = 'button'
        button.textContent = label
        nav.append(button)
      }

      const menu = document.createElement('div')
      menu.className = 'emoji-menu'
      menu.style.gridTemplateColumns = '1fr 1fr'
      root.append(menu)
      for (const emoji of ['😀', '😁', '😂', '🤣']) {
        const button = document.createElement('button')
        button.type = 'button'
        button.setAttribute('role', 'menuitem')
        button.textContent = emoji
        menu.append(button)
      }
    }
  }
  customElements.define('emoji-picker', Picker)
  return { Picker }
})

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const OTHER_ROOM = '!axontest:bostoncoop.net'
const TIMELINE_PATH = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/timeline`
const EVENTS_PATH = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`
const OWN_USER = '@me:hs'

function RoomsIndexStub() {
  return <div data-testid="rooms-index">Select a room</div>
}

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
    () => HttpResponse.json({ data: { events: [], next_cursor: null } }),
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
    () =>
      HttpResponse.json({
        data: { events: [], next_cursor: null },
      }),
  ),
  http.get(EVENTS_PATH, ({ params }) =>
    HttpResponse.json({ data: event(params.eventId as string, 0) }),
  ),
  // Drafts + read markers are hydrated on every room mount (M-W6 steps 5b/5c);
  // empty by default, and debounced writes are accepted.
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
  window.history.replaceState(null, '', '/')
})
afterAll(() => server.close())

function renderRoom(
  events: EventDto[],
  url?: string,
  DefaultRoute: () => JSX.Element = RoomPage,
) {
  server.use(
    http.get(TIMELINE_PATH, () =>
      HttpResponse.json({ data: { events, next_cursor: null } }),
    ),
  )
  window.history.replaceState(
    null,
    '',
    url ?? `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
  )
  // Fixtures here stamp events a few hundred ms past the epoch; put "now" just
  // after them so `reconcileSummary`'s recency window is not the thing under
  // test (it is exercised directly in `stores/thread-unread.test.ts`).
  const services = testServices({ now: () => 60_000 })
  const utils = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <Router>
          <Route path="/:accountId/rooms/:roomId" component={RoomPage} />
          <Route default component={DefaultRoute} />
        </Router>
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  return { services, ...utils }
}

function panelEventRow(panel: HTMLElement, eventId: string): HTMLElement {
  const row = [...panel.querySelectorAll('li.event-row')].find(
    (element) => element.getAttribute('data-event-id') === eventId,
  )
  if (!(row instanceof HTMLElement)) {
    throw new Error(`missing thread row ${eventId}`)
  }
  return row
}

function mockSinglePane() {
  return vi.spyOn(window, 'matchMedia').mockImplementation(
    (query: string) =>
      ({
        media: query,
        matches: query === SINGLE_PANE_QUERY,
        onchange: null,
        addListener: () => {},
        removeListener: () => {},
        addEventListener: () => {},
        removeEventListener: () => {},
        dispatchEvent: () => false,
      }) as MediaQueryList,
  )
}

// The defaults start clear of the left edge band that the browser's own
// swipe-back owns (`NATIVE_BACK_EDGE_PX`), which the handler declines.
function swipeRight(target: Element, startX = 90, endX = 198) {
  fireEvent.touchStart(target, {
    touches: [{ clientX: startX, clientY: 220 }],
  })
  fireEvent.touchEnd(target, {
    changedTouches: [{ clientX: endX, clientY: 226 }],
  })
}

describe('sending', () => {
  it('sends the composed message on Enter and clears the draft', async () => {
    let sendBody: Record<string, unknown> = {}
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        async ({ request }) => {
          sendBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$new' } })
        },
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$new', 200, { sender: OWN_USER, body: 'hello there' }),
        }),
      ),
    )
    const { services, findByLabelText, queryByText } = renderRoom([
      event('$1', 100),
    ])
    const clearSearch = vi.spyOn(services.search, 'clear')

    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement
    fireEvent.input(textarea, { target: { value: 'hello there' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    // The draft clears immediately (optimistic UI, doesn't wait on the
    // network); wait for reconciliation to fully settle too, so this test
    // doesn't leave an in-flight request for the next test's handlers.
    expect(textarea.value).toBe('')
    await waitFor(() => expect(sendBody.body).toBe('hello there'))
    expect(sendBody.thread_root).toBeNull()
    expect(clearSearch).toHaveBeenCalledOnce()
    await waitFor(() => expect(queryByText('Sending…')).toBeNull())
  })

  it('expands emoji shortcodes before sending a composed message', async () => {
    let sendBody: Record<string, unknown> = {}
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        async ({ request }) => {
          sendBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$new' } })
        },
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$new', 200, { sender: OWN_USER, body: '👍' }),
        }),
      ),
    )
    const { findByLabelText, queryByText } = renderRoom([event('$1', 100)])

    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement
    fireEvent.input(textarea, { target: { value: ':+1:' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(sendBody.body).toBe('👍'))
    expect(sendBody.formatted_body).toBeUndefined()
    await waitFor(() => expect(queryByText('Sending…')).toBeNull())
  })

  it('sends /literal without Markdown formatted_body', async () => {
    let sendBody: Record<string, unknown> = {}
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        async ({ request }) => {
          sendBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$new' } })
        },
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$new', 200, {
            sender: OWN_USER,
            body: '**not bold**',
          }),
        }),
      ),
    )
    const { findByLabelText, queryByText } = renderRoom([event('$1', 100)])

    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement
    fireEvent.input(textarea, { target: { value: '/literal **not bold**' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(sendBody.body).toBe('**not bold**'))
    expect(sendBody.formatted_body).toBeUndefined()
    expect(sendBody.format).toBeUndefined()
    await waitFor(() => expect(queryByText('Sending…')).toBeNull())
  })

  it.each([
    ['/html <b>bold</b><script>drop()</script>', 'bold', '<b>bold</b>'],
    [
      '/rainbow hi',
      'hi',
      '<font color="#ff0000">h</font><font color="#00ffff">i</font>',
    ],
    [
      '/spoiler CW | secret',
      'CW: secret (Spoiler)',
      '<span data-mx-spoiler="CW">secret</span>',
    ],
  ])(
    'sends %s as an explicit formatted message',
    async (command, plain, html) => {
      let sendBody: Record<string, unknown> = {}
      server.use(
        http.post(
          `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
          async ({ request }) => {
            sendBody = (await request.json()) as Record<string, unknown>
            return HttpResponse.json({ data: { event_id: '$new' } })
          },
        ),
        http.get(EVENTS_PATH, () =>
          HttpResponse.json({
            data: event('$new', 200, { sender: OWN_USER, body: plain }),
          }),
        ),
      )
      const { findByLabelText, queryByText } = renderRoom([event('$1', 100)])

      const textarea = (await findByLabelText(
        'Message Ops',
      )) as HTMLTextAreaElement
      fireEvent.input(textarea, { target: { value: command } })
      fireEvent.keyDown(textarea, { key: 'Enter' })

      await waitFor(() => expect(sendBody.body).toBe(plain))
      expect(sendBody.format).toBe('org.matrix.custom.html')
      expect(sendBody.formatted_body).toBe(html)
      await waitFor(() => expect(queryByText('Sending…')).toBeNull())
    },
  )

  it('answers a bare /spoiler with usage and keeps the composer text', async () => {
    const { findByLabelText, findByText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/spoiler' } })
    fireEvent.keyDown(textarea, { key: 'Escape' })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(await findByText('usage: /spoiler [reason |] <text>')).toBeTruthy()
    expect(textarea.value).toBe('/spoiler')
  })

  it('reply mode sets reply_to and shows a cancellable banner', async () => {
    let sendBody: Record<string, unknown> = {}
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        async ({ request }) => {
          sendBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$new' } })
        },
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$new', 200, { sender: OWN_USER, body: 'a reply' }),
        }),
      ),
    )
    const { findAllByRole, findByText, findByLabelText, queryByText } =
      renderRoom([event('$target', 100)])

    fireEvent.click((await findAllByRole('button', { name: 'Reply' }))[0])
    expect(await findByText('Replying to')).toBeTruthy()

    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement
    fireEvent.input(textarea, { target: { value: 'a reply' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(queryByText('Replying to')).toBeNull()
    await waitFor(() => expect(sendBody.reply_to).toBe('$target'))
    // Wait for reconciliation to settle so this test doesn't leave an
    // in-flight request for the next test's (reset) handlers.
    await waitFor(() => expect(queryByText('Sending…')).toBeNull())
  })

  it('escape cancels reply mode', async () => {
    const { findAllByRole, findByText, findByLabelText, queryByText } =
      renderRoom([event('$target', 100)])

    fireEvent.click((await findAllByRole('button', { name: 'Reply' }))[0])
    await findByText('Replying to')
    fireEvent.keyDown(await findByLabelText('Message Ops'), { key: 'Escape' })
    expect(queryByText('Replying to')).toBeNull()
  })

  it('/reply starts a reply to the latest visible message', async () => {
    let sendBody: Record<string, unknown> = {}
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        async ({ request }) => {
          sendBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$reply' } })
        },
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$reply', 300, { sender: OWN_USER, body: 'from banner' }),
        }),
      ),
    )
    const { container, findByLabelText, findByText, queryByText } = renderRoom([
      event('$latest', 200),
      event('$old', 100),
    ])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/reply' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(textarea.value).toBe('')
    expect(await findByText('Replying to')).toBeTruthy()
    expect(container.querySelector('.composer-banner')?.textContent).toContain(
      '$latest',
    )

    fireEvent.input(textarea, { target: { value: 'from banner' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(sendBody.reply_to).toBe('$latest'))
    expect(sendBody.body).toBe('from banner')
    await waitFor(() => expect(queryByText('Sending…')).toBeNull())
  })

  it('/reply with text sends an immediate reply to the latest visible message', async () => {
    let sendBody: Record<string, unknown> = {}
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        async ({ request }) => {
          sendBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$reply' } })
        },
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$reply', 300, {
            sender: OWN_USER,
            body: 'inline reply',
          }),
        }),
      ),
    )
    const { findByLabelText, queryByText } = renderRoom([
      event('$latest', 200),
      event('$old', 100),
    ])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/reply inline reply' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(sendBody.reply_to).toBe('$latest'))
    expect(sendBody.body).toBe('inline reply')
    expect(textarea.value).toBe('')
    expect(queryByText('Replying to')).toBeNull()
    await waitFor(() => expect(queryByText('Sending…')).toBeNull())
  })

  it('shows "Sending…" on the composed message immediately, before the POST resolves', async () => {
    let resolveSend: (value: Response) => void = () => {}
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        () => new Promise<Response>((resolve) => (resolveSend = resolve)),
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$new', 200, { sender: OWN_USER, body: 'hello there' }),
        }),
      ),
    )
    const { findByLabelText, findByText, queryByText } = renderRoom([
      event('$1', 100),
    ])

    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement
    fireEvent.input(textarea, { target: { value: 'hello there' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(textarea.value).toBe('')
    expect(await findByText('hello there')).toBeTruthy()
    expect(await findByText('Sending…')).toBeTruthy()

    resolveSend(HttpResponse.json({ data: { event_id: '$new' } }))
    await waitFor(() => expect(queryByText('Sending…')).toBeNull())
  })

  it('shows "Failed to send" with Retry/Discard, and Retry recovers', async () => {
    let attempts = 0
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        () => {
          attempts += 1
          return attempts === 1
            ? HttpResponse.json(
                { error: { code: 'server_not_ready', message: 'nope' } },
                { status: 503 },
              )
            : HttpResponse.json({ data: { event_id: '$new' } })
        },
      ),
      http.get(EVENTS_PATH, () =>
        HttpResponse.json({
          data: event('$new', 200, { sender: OWN_USER, body: 'retry me' }),
        }),
      ),
    )
    const { findByLabelText, findByText, findByRole } = renderRoom([
      event('$1', 100),
    ])

    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement
    fireEvent.input(textarea, { target: { value: 'retry me' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(await findByText('Failed to send')).toBeTruthy()
    fireEvent.click(await findByRole('button', { name: 'Retry' }))

    await waitFor(() => expect(attempts).toBe(2))
    expect(await findByText('retry me')).toBeTruthy()
  })

  it('Discard removes the failed row', async () => {
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        () =>
          HttpResponse.json(
            { error: { code: 'server_not_ready', message: 'nope' } },
            { status: 503 },
          ),
      ),
    )
    const { findByLabelText, findByText, findByRole, queryByText } = renderRoom(
      [event('$1', 100)],
    )

    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement
    fireEvent.input(textarea, { target: { value: 'doomed' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(await findByText('Failed to send')).toBeTruthy()
    fireEvent.click(await findByRole('button', { name: 'Discard' }))

    await waitFor(() => expect(queryByText('doomed')).toBeNull())
  })
})

describe('editing and redacting', () => {
  it('edit is offered only on own messages, prefills, and PUTs', async () => {
    let editBody: Record<string, unknown> = {}
    server.use(
      http.put(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId`,
        async ({ request, params }) => {
          editBody = (await request.json()) as Record<string, unknown>
          expect(params.eventId).toBe('$mine')
          return HttpResponse.json({ data: { event_id: '$edit' } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({
          data: event('$mine', 200, { sender: OWN_USER, body: 'fixed' }),
        }),
      ),
    )
    const { findAllByRole, getByLabelText, findByText } = renderRoom([
      event('$theirs', 100),
      event('$mine', 200, { sender: OWN_USER, body: 'typo here' }),
    ])

    await findByText('typo here')
    // Own-message detection needs the rooms fetch (account_user_id), so wait
    // for the button rather than querying synchronously. Only one exists —
    // the other message is not ours.
    const editButtons = await findAllByRole('button', { name: 'Edit' })
    expect(editButtons).toHaveLength(1)

    fireEvent.click(editButtons[0])
    const textarea = getByLabelText('Message Ops') as HTMLTextAreaElement
    expect(textarea.value).toBe('typo here')

    fireEvent.input(textarea, { target: { value: 'fixed' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(editBody.body).toBe('fixed'))
    expect(await findByText('fixed')).toBeTruthy()
  })

  it('a failed edit re-opens edit mode with the typed text (WCR-10)', async () => {
    server.use(
      http.put(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId`,
        () =>
          HttpResponse.json(
            { error: { code: 'internal', message: 'db unavailable' } },
            { status: 500 },
          ),
      ),
    )
    const { findAllByRole, getByLabelText, findByText } = renderRoom([
      event('$mine', 200, { sender: OWN_USER, body: 'typo here' }),
    ])

    const editButtons = await findAllByRole('button', { name: 'Edit' })
    fireEvent.click(editButtons[0])
    const textarea = getByLabelText('Message Ops') as HTMLTextAreaElement
    expect(textarea.value).toBe('typo here')

    fireEvent.input(textarea, { target: { value: 'my careful rewrite' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    // The PUT failed: the error surfaces AND edit mode returns with the text
    // the user typed — not the original body, and not an empty composer.
    expect(await findByText('db unavailable')).toBeTruthy()
    expect(await findByText('Editing')).toBeTruthy()
    await waitFor(() =>
      expect((getByLabelText('Message Ops') as HTMLTextAreaElement).value).toBe(
        'my careful rewrite',
      ),
    )
  })

  it('redact requires confirmation and masks the row', async () => {
    let deleted: string | null = null
    server.use(
      http.delete(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId`,
        ({ params }) => {
          deleted = params.eventId as string
          return new HttpResponse(null, { status: 204 })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({
          data: event('$mine', 100, {
            sender: OWN_USER,
            redacted: true,
            body: null,
            content: null,
          }),
        }),
      ),
    )
    const { services, findAllByRole, findByRole, findByText } = renderRoom([
      event('$mine', 100, { sender: OWN_USER }),
    ])
    const clearSearch = vi.spyOn(services.search, 'clear')

    fireEvent.click((await findAllByRole('button', { name: 'Delete' }))[0])
    expect(deleted).toBeNull()
    fireEvent.click(await findByRole('button', { name: 'Confirm delete' }))

    await waitFor(() => expect(deleted).toBe('$mine'))
    expect(clearSearch).toHaveBeenCalledOnce()
    expect(await findByText('message deleted')).toBeTruthy()
  })
})

describe('reactions', () => {
  it('quick-picker reacts and chip click toggles off', async () => {
    let posted: unknown
    let deleted: string | null = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        async ({ request }) => {
          posted = await request.json()
          return HttpResponse.json({ data: { event_id: '$rx' } })
        },
      ),
      http.delete(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId`,
        ({ params }) => {
          deleted = params.eventId as string
          return new HttpResponse(null, { status: 204 })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({
          data: event('$msg', 100, {
            reactions: {
              '👍': {
                count: 1,
                me: true,
                senders: [OWN_USER],
                my_event_ids: ['$rx'],
              },
            },
          }),
        }),
      ),
    )
    const { services, findAllByRole, findByRole, findByText } = renderRoom([
      event('$msg', 100),
    ])
    const clearSearch = vi.spyOn(services.search, 'clear')

    fireEvent.click((await findAllByRole('button', { name: 'React' }))[0])
    fireEvent.click(await findByRole('button', { name: '👍' }))
    await waitFor(() => expect(posted).toEqual({ key: '👍' }))
    expect(clearSearch).toHaveBeenCalledOnce()

    // The refreshed row now shows my chip; clicking it redacts my reaction.
    fireEvent.click(await findByText('👍 1'))
    await waitFor(() => expect(deleted).toBe('$rx'))
  })

  it('Escape dismisses the reaction picker', async () => {
    const { findAllByRole, queryByRole } = renderRoom([event('$msg', 100)])

    fireEvent.click((await findAllByRole('button', { name: 'React' }))[0])
    expect(await findAllByRole('group', { name: 'React with' })).toHaveLength(1)

    fireEvent.keyDown(document.body, { key: 'Escape' })

    expect(queryByRole('group', { name: 'React with' })).toBeNull()
  })

  it('/react opens the reaction picker on the latest visible message', async () => {
    const { findByLabelText, container } = renderRoom([
      event('$latest', 200),
      event('$old', 100),
    ])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/react' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(textarea.value).toBe('')
    await waitFor(() => expect(document.activeElement?.textContent).toBe('👍'))
    expect(
      container
        .querySelector('li.event-row[data-event-id="$latest"]')
        ?.querySelector('.reaction-picker'),
    ).not.toBeNull()
    expect(
      container
        .querySelector('li.event-row[data-event-id="$old"]')
        ?.querySelector('.reaction-picker'),
    ).toBeNull()
  })

  it('/react with an emoji reacts to the latest visible message', async () => {
    let posted: unknown
    let reactedEventId: string | null = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        async ({ params, request }) => {
          reactedEventId = params.eventId as string
          posted = await request.json()
          return HttpResponse.json({ data: { event_id: '$rx' } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$latest', 200) }),
      ),
    )
    const { findByLabelText } = renderRoom([
      event('$latest', 200),
      event('$old', 100),
    ])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/react 🔥' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(reactedEventId).toBe('$latest'))
    expect(posted).toEqual({ key: '🔥' })
    expect(textarea.value).toBe('')
  })

  it('/react with an emoji reacts to the latest visible thread-pane message from the thread composer', async () => {
    let posted: unknown
    let reactedEventId: string | null = null
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                event('$thread-latest', 250, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'newer reply',
                }),
                event('$thread-old', 150, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'older reply',
                }),
              ],
              next_cursor: null,
            },
          }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$thread-latest', 250) }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        async ({ params, request }) => {
          reactedEventId = params.eventId as string
          posted = await request.json()
          return HttpResponse.json({ data: { event_id: '$rx' } })
        },
      ),
    )
    const { findByLabelText } = renderRoom(
      [event('$room-latest', 300), event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    const panel = await findByLabelText('Thread')
    await within(panel).findByText('newer reply')
    const textarea = within(panel).getByLabelText(
      'Reply in thread',
    ) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/react 🔥' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(reactedEventId).toBe('$thread-latest'))
    expect(posted).toEqual({ key: '🔥' })
    expect(textarea.value).toBe('')
  })

  it('/+ aliases /react for the latest visible message', async () => {
    let posted: unknown
    let reactedEventId: string | null = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        async ({ params, request }) => {
          reactedEventId = params.eventId as string
          posted = await request.json()
          return HttpResponse.json({ data: { event_id: '$rx' } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$latest', 200) }),
      ),
    )
    const { findByLabelText } = renderRoom([
      event('$latest', 200),
      event('$old', 100),
    ])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/+ 🔥' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(reactedEventId).toBe('$latest'))
    expect(posted).toEqual({ key: '🔥' })
    expect(textarea.value).toBe('')
  })

  it('/react resolves common shortcode aliases', async () => {
    let posted: unknown
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        async ({ request }) => {
          posted = await request.json()
          return HttpResponse.json({ data: { event_id: '$rx' } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$msg', 100) }),
      ),
    )
    const { findByLabelText } = renderRoom([event('$msg', 100)])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/react :thumbs_up:' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(posted).toEqual({ key: '👍' }))
  })

  it('/react resolves emoji data shortcodes', async () => {
    let posted: unknown
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        async ({ request }) => {
          posted = await request.json()
          return HttpResponse.json({ data: { event_id: '$rx' } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$msg', 100) }),
      ),
    )
    const { findByLabelText } = renderRoom([event('$msg', 100)])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/react partying' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(posted).toEqual({ key: '🥳' }))
  })

  it('opens the full emoji picker as a detached floating dialog', async () => {
    const { findAllByRole, findByRole, container } = renderRoom([
      event('$msg', 100),
    ])

    fireEvent.click((await findAllByRole('button', { name: 'React' }))[0])
    fireEvent.click(await findByRole('button', { name: 'More reactions' }))

    const shell = container.querySelector('.reaction-picker-shell')
    const compactPicker = container.querySelector('.reaction-picker')
    const fullPicker = await findByRole('dialog', { name: 'Emoji picker' })
    expect(fullPicker).toBeTruthy()
    expect(fullPicker.parentElement).toBe(document.body)
    expect(shell?.contains(fullPicker)).toBe(false)
    expect(fullPicker.parentElement).not.toBe(compactPicker)
    expect(compactPicker?.contains(fullPicker)).toBe(false)
    await waitFor(() =>
      expect(fullPicker.querySelector('emoji-picker')).not.toBeNull(),
    )
    expect(pickerProps.at(-1)).toEqual({
      dataSource: '/emoji-picker-data-en.json',
    })
  })

  it('opens the full emoji picker with + and focuses emoji search', async () => {
    const { findAllByRole, findByRole } = renderRoom([event('$msg', 100)])

    fireEvent.click((await findAllByRole('button', { name: 'React' }))[0])
    fireEvent.keyDown(document, { key: '+' })

    const fullPicker = await findByRole('dialog', { name: 'Emoji picker' })
    await waitFor(() =>
      expect(fullPicker.querySelector('emoji-picker')).not.toBeNull(),
    )
    const picker = fullPicker.querySelector('emoji-picker') as HTMLElement
    await waitFor(() =>
      expect(picker.shadowRoot?.activeElement).toBe(
        picker.shadowRoot?.querySelector('input[type="search"]'),
      ),
    )
  })

  it('tabs through reaction controls inside the picker instead of leaving the overlay', async () => {
    const { findAllByRole, findByRole } = renderRoom([event('$msg', 100)])

    fireEvent.click((await findAllByRole('button', { name: 'React' }))[0])
    fireEvent.click(await findByRole('button', { name: 'More reactions' }))

    const fullPicker = await findByRole('dialog', { name: 'Emoji picker' })
    const picker = await waitFor(() => {
      const element = fullPicker.querySelector('emoji-picker')
      expect(element).toBeTruthy()
      return element as HTMLElement
    })
    const shadow = picker.shadowRoot!
    const search = shadow.querySelector('input[type="search"]') as HTMLElement
    const firstNav = shadow.querySelector('.nav-button') as HTMLElement

    await waitFor(() => expect(shadow.activeElement).toBe(search))
    fireEvent.keyDown(document, { key: 'Tab' })
    expect(shadow.activeElement).toBe(firstNav)
    await waitFor(() =>
      expect(firstNav.getAttribute('data-focus-visible-added')).toBe(''),
    )
  })

  it('moves through full-picker emoji buttons with arrow keys', async () => {
    const { findAllByRole, findByRole } = renderRoom([event('$msg', 100)])

    fireEvent.click((await findAllByRole('button', { name: 'React' }))[0])
    fireEvent.click(await findByRole('button', { name: 'More reactions' }))

    const fullPicker = await findByRole('dialog', { name: 'Emoji picker' })
    const picker = await waitFor(() => {
      const element = fullPicker.querySelector('emoji-picker')
      expect(element).toBeTruthy()
      return element as HTMLElement
    })
    const shadow = picker.shadowRoot!
    const buttons = [
      ...shadow.querySelectorAll<HTMLButtonElement>('button[role="menuitem"]'),
    ]

    buttons[0].focus()
    expect(shadow.activeElement).toBe(buttons[0])

    fireEvent.keyDown(picker, { key: 'ArrowRight' })
    expect(shadow.activeElement).toBe(buttons[1])
    await waitFor(() =>
      expect(buttons[1].getAttribute('data-focus-visible-added')).toBe(''),
    )

    fireEvent.keyDown(picker, { key: 'ArrowDown' })
    expect(shadow.activeElement).toBe(buttons[3])
    await waitFor(() =>
      expect(buttons[3].getAttribute('data-focus-visible-added')).toBe(''),
    )
  })

  it('dismisses the full emoji picker with an explicit close button', async () => {
    const { findAllByRole, findByRole, queryByRole } = renderRoom([
      event('$msg', 100),
    ])

    fireEvent.click((await findAllByRole('button', { name: 'React' }))[0])
    fireEvent.click(await findByRole('button', { name: 'More reactions' }))
    expect(await findByRole('dialog', { name: 'Emoji picker' })).toBeTruthy()

    fireEvent.click(await findByRole('button', { name: 'Close picker' }))

    expect(queryByRole('dialog', { name: 'Emoji picker' })).toBeNull()
    expect(queryByRole('group', { name: 'React with' })).toBeNull()
  })

  it('adds the selected full-picker emoji to recent compact reactions', async () => {
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        () => HttpResponse.json({ data: { event_id: '$rx' } }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$msg', 100) }),
      ),
    )
    const { services, findAllByRole, findByRole } = renderRoom([
      event('$msg', 100),
    ])

    fireEvent.click((await findAllByRole('button', { name: 'React' }))[0])
    fireEvent.click(await findByRole('button', { name: 'More reactions' }))
    const dialog = await findByRole('dialog', { name: 'Emoji picker' })
    await waitFor(() =>
      expect(dialog.querySelector('emoji-picker')).not.toBeNull(),
    )
    dialog.querySelector('emoji-picker')?.dispatchEvent(
      new CustomEvent('emoji-click', {
        bubbles: true,
        detail: { unicode: '🔥' },
      }),
    )

    expect(services.settings.recentReactions.value).toEqual(['🔥'])

    fireEvent.click((await findAllByRole('button', { name: 'React' }))[0])
    expect(await findByRole('button', { name: '🔥' })).toBeTruthy()
  })

  it('canonicalizes selected full-picker emoji variants', async () => {
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        () => HttpResponse.json({ data: { event_id: '$rx' } }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$msg', 100) }),
      ),
    )
    const { services, findAllByRole, findByRole } = renderRoom([
      event('$msg', 100),
    ])

    fireEvent.click((await findAllByRole('button', { name: 'React' }))[0])
    fireEvent.click(await findByRole('button', { name: 'More reactions' }))
    const dialog = await findByRole('dialog', { name: 'Emoji picker' })
    await waitFor(() =>
      expect(dialog.querySelector('emoji-picker')).not.toBeNull(),
    )
    dialog.querySelector('emoji-picker')?.dispatchEvent(
      new CustomEvent('emoji-click', {
        bubbles: true,
        detail: { unicode: '👍️' },
      }),
    )

    expect(services.settings.recentReactions.value).toEqual(['👍'])
  })

  it('shows defaults plus the three most recent non-default reactions', async () => {
    const { services, findAllByRole, findByRole, queryByRole } = renderRoom([
      event('$msg', 100),
    ])
    services.settings.recordRecentReaction('🔥')
    services.settings.recordRecentReaction('🦝')
    services.settings.recordRecentReaction('👍')
    services.settings.recordRecentReaction('🚀')

    fireEvent.click((await findAllByRole('button', { name: 'React' }))[0])

    expect(await findByRole('button', { name: '👍' })).toBeTruthy()
    expect(await findByRole('button', { name: '🚀' })).toBeTruthy()
    expect(await findByRole('button', { name: '🦝' })).toBeTruthy()
    expect(queryByRole('button', { name: '🔥' })).toBeNull()
  })
})

describe('threads', () => {
  it('opens a new thread from the message action row', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: { events: [], next_cursor: null },
          }),
      ),
    )
    const { findAllByRole, findAllByText, findByLabelText, getByLabelText } =
      renderRoom([event('$root', 100)])

    fireEvent.click((await findAllByRole('button', { name: 'Thread' }))[0])

    expect(await findByLabelText('Thread')).toBeTruthy()
    expect(await findAllByText('body of $root')).toHaveLength(2)
    const threadComposer = getByLabelText(
      'Reply in thread',
    ) as HTMLTextAreaElement
    await waitFor(() => expect(document.activeElement).toBe(threadComposer))
  })

  it('inserts day separators when a thread spans multiple days', async () => {
    const day = 86_400_000
    const monday = Date.UTC(2026, 5, 1, 12, 0, 0)
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              // Newest-first, matching the read API; the store reverses to
              // oldest-at-top the same way the room timeline does.
              events: [
                event('$m2', monday + day, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'next day',
                }),
                event('$m1', monday, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'same day as root',
                }),
              ],
              next_cursor: null,
            },
          }),
      ),
    )
    const { findByLabelText } = renderRoom(
      [event('$root', monday)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    const panel = await findByLabelText('Thread')
    await within(panel).findByText('next day')
    await waitFor(() => panelEventRow(panel, '$root'))

    const labels = [...panel.querySelectorAll('.day-separator')].map(
      (el) => el.textContent,
    )
    expect(labels).toHaveLength(2)
    expect(labels[0]).not.toBe(labels[1])
  })

  it('does not insert a second day heading when thread replies stay on the root day', async () => {
    const monday = Date.UTC(2026, 5, 1, 12, 0, 0)
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                event('$m1', monday + 60_000, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'later that day',
                }),
              ],
              next_cursor: null,
            },
          }),
      ),
    )
    const { findByLabelText } = renderRoom(
      [event('$root', monday)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    const panel = await findByLabelText('Thread')
    await within(panel).findByText('later that day')
    await waitFor(() => panelEventRow(panel, '$root'))

    expect(panel.querySelectorAll('.day-separator')).toHaveLength(1)
  })

  it('mobile swipe-right from a thread closes the thread panel', async () => {
    const media = mockSinglePane()
    try {
      const { findByLabelText, queryByLabelText } = renderRoom(
        [event('$root', 100)],
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
      )
      const panel = await findByLabelText('Thread')

      swipeRight(panel)

      await waitFor(() => expect(queryByLabelText('Thread')).toBeNull())
      expect(window.location.search).toBe('')
    } finally {
      media.mockRestore()
    }
  })

  it('closing the thread panel via its Close button does not add a history entry, so a subsequent back goes to the room list', async () => {
    const media = mockSinglePane()
    function RoomListStub() {
      return <p>Rooms list</p>
    }
    const roomUrl = `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`
    try {
      // Mirror the real flow: RoomList's navigateToRoom pushes the room URL
      // on top of the room-list entry, rather than replacing it.
      window.history.replaceState(null, '', '/')
      window.history.pushState(null, '', roomUrl)

      const { findAllByRole, findByLabelText, findByRole, findByText } =
        renderRoom([event('$root', 100)], roomUrl, RoomListStub)
      await findByLabelText('Message Ops')
      const historyLengthBeforeThread = window.history.length

      fireEvent.click((await findAllByRole('button', { name: 'Thread' }))[0])
      await findByLabelText('Thread')

      fireEvent.click(await findByRole('button', { name: 'Close' }))
      await waitFor(() => expect(window.location.search).toBe(''))

      // Opening and closing the thread pane must not have pushed any new
      // history entries — it's pane state, not a page.
      expect(window.history.length).toBe(historyLengthBeforeThread)

      window.history.back()

      expect(await findByText('Rooms list')).toBeTruthy()
    } finally {
      media.mockRestore()
    }
  })

  it('opening room information closes the thread panel', async () => {
    const { findByLabelText, findByRole, queryByLabelText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    expect(await findByLabelText('Thread')).toBeTruthy()

    fireEvent.click(
      await findByRole('button', { name: 'Open room information' }),
    )

    expect(await findByLabelText('Room information')).toBeTruthy()
    await waitFor(() => expect(queryByLabelText('Thread')).toBeNull())
  })

  it('opening a thread closes room information', async () => {
    const { findAllByRole, findByLabelText, findByRole, queryByLabelText } =
      renderRoom([event('$root', 100)])

    fireEvent.click(
      await findByRole('button', { name: 'Open room information' }),
    )
    expect(await findByLabelText('Room information')).toBeTruthy()

    fireEvent.click((await findAllByRole('button', { name: 'Thread' }))[0])

    expect(await findByLabelText('Thread')).toBeTruthy()
    await waitFor(() => expect(queryByLabelText('Room information')).toBeNull())
  })

  it('mobile swipe-right from the main timeline returns to the room list', async () => {
    const media = mockSinglePane()
    function RoomListStub() {
      return <p>Rooms list</p>
    }
    try {
      const { container, findByLabelText, findByText } = renderRoom(
        [event('$root', 100)],
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
        RoomListStub,
      )
      await findByLabelText('Message Ops')

      swipeRight(container.querySelector('.room-body')!)

      expect(await findByText('Rooms list')).toBeTruthy()
      expect(window.location.pathname).toBe('/')
    } finally {
      media.mockRestore()
    }
  })

  it('mobile swipe-right starting in the composer does not navigate', async () => {
    const media = mockSinglePane()
    try {
      const { findByLabelText } = renderRoom(
        [event('$root', 100)],
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
      )
      const composer = await findByLabelText('Message Ops')

      swipeRight(composer)

      expect(window.location.pathname).toContain('/rooms/')
    } finally {
      media.mockRestore()
    }
  })

  it('mobile swipe-right starting inside a horizontally scrollable element (e.g. a wide code block) does not navigate', async () => {
    const media = mockSinglePane()
    try {
      const { container, findByLabelText } = renderRoom(
        [event('$root', 100)],
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
      )
      await findByLabelText('Message Ops')

      const scrollable = container.querySelector('.room-stream') as HTMLElement
      scrollable.style.overflowX = 'auto'
      Object.defineProperty(scrollable, 'scrollWidth', {
        value: 400,
        configurable: true,
      })
      Object.defineProperty(scrollable, 'clientWidth', {
        value: 200,
        configurable: true,
      })

      swipeRight(scrollable)

      expect(window.location.pathname).toContain('/rooms/')
    } finally {
      media.mockRestore()
    }
  })

  it('mobile swipe-right claims the touchmove once a rightward drag is detected, so scrolling and selection do not fight the pan', async () => {
    const media = mockSinglePane()
    try {
      const { container, findByLabelText } = renderRoom(
        [event('$root', 100)],
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
      )
      await findByLabelText('Message Ops')
      const body = container.querySelector('.room-body')!

      fireEvent.touchStart(body, { touches: [{ clientX: 90, clientY: 220 }] })
      const notPrevented = fireEvent.touchMove(body, {
        touches: [{ clientX: 126, clientY: 222 }],
      })

      expect(notPrevented).toBe(false)
    } finally {
      media.mockRestore()
    }
  })

  // A swipe from the left edge races the browser's own back gesture, which
  // ignores `preventDefault`: both fire, and the page then stalls ~500ms
  // without painting while the browser animates a stale snapshot. Declining
  // the band is what keeps that to a single navigation (ADR 0075).
  it('mobile swipe-right starting in the browser back-gesture edge band does not navigate', async () => {
    const media = mockSinglePane()
    try {
      const { container, findByLabelText } = renderRoom(
        [event('$root', 100)],
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
      )
      await findByLabelText('Message Ops')
      const body = container.querySelector('.room-body')!

      // Same travel as an accepted swipe — only the origin differs.
      swipeRight(body, 12, 120)

      expect(window.location.pathname).toContain('/rooms/')
    } finally {
      media.mockRestore()
    }
  })

  it('mobile swipe-right in the edge band leaves the touchmove unclaimed for the browser', async () => {
    const media = mockSinglePane()
    try {
      const { container, findByLabelText } = renderRoom(
        [event('$root', 100)],
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
      )
      await findByLabelText('Message Ops')
      const body = container.querySelector('.room-body')!

      fireEvent.touchStart(body, { touches: [{ clientX: 12, clientY: 220 }] })
      const notPrevented = fireEvent.touchMove(body, {
        touches: [{ clientX: 48, clientY: 222 }],
      })

      expect(notPrevented).toBe(true)
    } finally {
      media.mockRestore()
    }
  })

  it('/thread opens a thread on the latest visible message', async () => {
    let openedRoot: string | null = null
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        ({ params }) => {
          openedRoot = params.rootId as string
          return HttpResponse.json({
            data: { events: [], next_cursor: null },
          })
        },
      ),
    )
    const { findAllByText, findByLabelText } = renderRoom([
      event('$latest', 200),
      event('$old', 100),
    ])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/thread' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(openedRoot).toBe('$latest'))
    expect(await findByLabelText('Thread')).toBeTruthy()
    expect(await findAllByText('body of $latest')).toHaveLength(2)
    expect(textarea.value).toBe('')
    const threadComposer = (await findByLabelText(
      'Reply in thread',
    )) as HTMLTextAreaElement
    await waitFor(() => expect(document.activeElement).toBe(threadComposer))
  })

  it('hides thread members from the main timeline and badges the root', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`,
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
        HttpResponse.json({ data: event('$root', 100) }),
      ),
    )
    const { findByText, queryByText } = renderRoom([
      event('$root', 100),
      event('$m1', 200, {
        relates_to: { rel_type: 'm.thread', event_id: '$root' },
        body: 'thread reply one',
      }),
    ])

    expect(await findByText('body of $root')).toBeTruthy()
    expect(await findByText('💬 2 replies')).toBeTruthy()
    expect(queryByText('thread reply one')).toBeNull()
  })

  it('keeps hidden thread replies unread until the thread panel loads', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`,
        ({ params }) => {
          if (params.namespace === 'read_markers') {
            return HttpResponse.json({
              data: {
                namespace: 'read_markers',
                entries: {
                  [ROOM]: {
                    value: { event_id: '$before', origin_ts: 150 },
                    device_id: 'this-device',
                    updated_at: '2026-01-01T00:00:00Z',
                  },
                },
              },
            })
          }
          return HttpResponse.json({
            data: { namespace: params.namespace, entries: {} },
          })
        },
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`,
        () =>
          HttpResponse.json({
            data: [
              {
                root_event_id: '$root',
                reply_count: 1,
                latest_reply_event_id: '$m1',
                latest_reply_ts: 200,
              },
            ],
          }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                event('$m1', 200, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'thread reply one',
                }),
              ],
              next_cursor: null,
            },
          }),
      ),
    )
    const { findByRole, findByText, queryByText } = renderRoom([
      event('$root', 100),
      event('$m1', 200, {
        relates_to: { rel_type: 'm.thread', event_id: '$root' },
        body: 'thread reply one',
      }),
    ])

    expect(await findByText('New')).toBeTruthy()
    expect(queryByText('thread reply one')).toBeNull()

    fireEvent.click(await findByRole('button', { name: /1 reply/ }))
    expect(await findByText('thread reply one')).toBeTruthy()
    await waitFor(() => expect(queryByText('New')).toBeNull())
  })

  it('opens a thread from the replies badge on touch pointer down', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`,
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
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () => HttpResponse.json({ data: { events: [], next_cursor: null } }),
      ),
    )
    const { findByText, findByLabelText } = renderRoom([event('$root', 100)])

    const badge = await findByText('💬 2 replies')
    fireEvent.pointerDown(badge, { pointerType: 'touch' })

    expect(await findByLabelText('Thread')).toBeTruthy()
  })

  it('?thread= opens the panel with the thread timeline and sends into it', async () => {
    let sendBody: Record<string, unknown> = {}
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                event('$m1', 200, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'inside the thread',
                  content: { msgtype: 'm.text', body: 'inside the thread' },
                }),
              ],
              next_cursor: null,
            },
          }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$root', 100) }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        async ({ request }) => {
          sendBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$new' } })
        },
      ),
    )
    const { findByText, findAllByText, getByLabelText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )

    expect(await findByText('inside the thread')).toBeTruthy()
    // The root body renders in the main timeline, the desktop room stream, and
    // the panel head. A thread deep link must not leave the room stream empty.
    expect(await findAllByText('body of $root')).toHaveLength(3)

    const textarea = getByLabelText('Reply in thread') as HTMLTextAreaElement
    fireEvent.input(textarea, { target: { value: 'thread send' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(sendBody.thread_root).toBe('$root'))
    expect(sendBody.reply_to).toBeNull()
    expect(sendBody.body).toBe('thread send')
  })

  it('/thread from the thread composer does not retarget the open thread', async () => {
    const threadRequests: string[] = []
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        ({ params }) => {
          threadRequests.push(params.rootId as string)
          return HttpResponse.json({
            data: {
              events: [
                event('$m1', 200, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'inside the thread',
                  content: { msgtype: 'm.text', body: 'inside the thread' },
                }),
              ],
              next_cursor: null,
            },
          })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$root', 100) }),
      ),
    )
    const { findByLabelText, findByText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )

    expect(await findByText('inside the thread')).toBeTruthy()
    threadRequests.length = 0

    const textarea = (await findByLabelText(
      'Reply in thread',
    )) as HTMLTextAreaElement
    fireEvent.input(textarea, { target: { value: '/thread' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(textarea.value).toBe(''))
    expect(window.location.search).toBe('?thread=%24root')
    expect(threadRequests).toEqual([])
  })

  it('hides redacted thread replies when the setting is on', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                event('$m1', 200, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'inside the thread',
                  content: { msgtype: 'm.text', body: 'inside the thread' },
                }),
                event('$m2', 300, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  redacted: true,
                  content: null,
                  body: null,
                  redaction_event_id: '$redaction',
                }),
              ],
              next_cursor: null,
            },
          }),
      ),
    )
    const { services, findByText, queryByText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )

    expect(await findByText('inside the thread')).toBeTruthy()
    expect(await findByText('message deleted')).toBeTruthy()

    services.settings.hideRedactedEvents.value = true

    await waitFor(() => expect(queryByText('message deleted')).toBeNull())
    expect(await findByText('inside the thread')).toBeTruthy()
  })

  it('switches an already-open thread panel to the clicked thread root', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`,
        () =>
          HttpResponse.json({
            data: [
              {
                root_event_id: '$root1',
                reply_count: 1,
                latest_reply_event_id: '$root1-reply',
                latest_reply_ts: 300,
              },
              {
                root_event_id: '$root2',
                reply_count: 1,
                latest_reply_event_id: '$root2-reply',
                latest_reply_ts: 400,
              },
            ],
          }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        ({ params }) => {
          const rootId = params.rootId as string
          return HttpResponse.json({
            data: {
              events: [
                event(`${rootId}-reply`, 500, {
                  relates_to: {
                    rel_type: 'm.thread',
                    event_id: rootId,
                  },
                  body: `reply for ${rootId}`,
                }),
              ],
              next_cursor: null,
            },
          })
        },
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        ({ params }) =>
          HttpResponse.json({ data: event(params.eventId as string, 200) }),
      ),
    )
    const { findAllByRole, findByLabelText } = renderRoom([
      event('$root2', 200),
      event('$root1', 100),
    ])

    const badges = await findAllByRole('button', { name: /1 reply/ })
    fireEvent.click(badges[0])
    const panel = await findByLabelText('Thread')
    expect(await within(panel).findByText('reply for $root1')).toBeTruthy()

    fireEvent.click(badges[1])
    const switchedPanel = await findByLabelText('Thread')

    expect(
      await within(switchedPanel).findByText('reply for $root2'),
    ).toBeTruthy()
    expect(within(switchedPanel).queryByText('reply for $root1')).toBeNull()
    expect(within(switchedPanel).queryByText('body of $root1')).toBeNull()
    expect(within(switchedPanel).getByText('body of $root2')).toBeTruthy()
  })

  it('replying from the thread pane keeps the thread root and sets reply_to', async () => {
    let sendBody: Record<string, unknown> = {}
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                event('$m1', 200, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'inside the thread',
                  content: { msgtype: 'm.text', body: 'inside the thread' },
                }),
              ],
              next_cursor: null,
            },
          }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$root', 100) }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send`,
        async ({ request }) => {
          sendBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$new' } })
        },
      ),
    )
    const { findByLabelText, getByLabelText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    const panel = await findByLabelText('Thread')
    await within(panel).findByText('inside the thread')

    fireEvent.click(
      within(panelEventRow(panel, '$m1')).getByRole('button', {
        name: 'Reply',
      }),
    )
    const textarea = getByLabelText('Reply in thread') as HTMLTextAreaElement
    fireEvent.input(textarea, { target: { value: 'specific reply' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(sendBody.body).toBe('specific reply'))
    expect(sendBody.thread_root).toBe('$root')
    expect(sendBody.reply_to).toBe('$m1')
  })

  it('reacts to a thread-pane message', async () => {
    let reactionBody: Record<string, unknown> = {}
    let reactionTarget: string | null = null
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                event('$m1', 200, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'inside the thread',
                }),
              ],
              next_cursor: null,
            },
          }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$m1', 200) }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        async ({ params, request }) => {
          reactionTarget = params.eventId as string
          reactionBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$reaction' } })
        },
      ),
    )
    const { findByLabelText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    const panel = await findByLabelText('Thread')
    await within(panel).findByText('inside the thread')

    fireEvent.click(
      within(panelEventRow(panel, '$m1')).getByRole('button', {
        name: 'React',
      }),
    )
    fireEvent.click(
      within(panelEventRow(panel, '$m1')).getByRole('button', { name: '👍' }),
    )

    await waitFor(() => expect(reactionBody.key).toBe('👍'))
    expect(reactionTarget).toBe('$m1')
  })

  it('lets the thread root use message actions except opening a nested thread', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                event('$m1', 200, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'inside the thread',
                }),
              ],
              next_cursor: null,
            },
          }),
      ),
    )
    const { findByLabelText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    const panel = await findByLabelText('Thread')
    await within(panel).findByText('inside the thread')
    const rootRow = await waitFor(() => panelEventRow(panel, '$root'))

    expect(within(rootRow).getByRole('button', { name: 'Reply' })).toBeTruthy()
    expect(within(rootRow).getByRole('button', { name: 'React' })).toBeTruthy()
    expect(within(rootRow).queryByRole('button', { name: 'Thread' })).toBeNull()
    expect(within(panel).queryByRole('button', { name: 'Thread' })).toBeNull()
  })

  it('reacts to the thread root from the thread pane', async () => {
    let reactionBody: Record<string, unknown> = {}
    let reactionTarget: string | null = null
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: { events: [], next_cursor: null },
          }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        async ({ params, request }) => {
          reactionTarget = params.eventId as string
          reactionBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$reaction' } })
        },
      ),
    )
    const { findByLabelText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    const panel = await findByLabelText('Thread')
    const rootRow = await waitFor(() => panelEventRow(panel, '$root'))

    fireEvent.click(
      within(rootRow).getByRole('button', {
        name: 'React',
      }),
    )
    fireEvent.click(within(panel).getByRole('button', { name: '👍' }))

    await waitFor(() => expect(reactionBody.key).toBe('👍'))
    expect(reactionTarget).toBe('$root')
  })

  it('edits an own message from the thread pane', async () => {
    let editBody: Record<string, unknown> = {}
    let editTarget: string | null = null
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                event('$mine', 200, {
                  sender: OWN_USER,
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'thread typo',
                }),
              ],
              next_cursor: null,
            },
          }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$mine', 200, { sender: OWN_USER }) }),
      ),
      http.put(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId`,
        async ({ params, request }) => {
          editTarget = params.eventId as string
          editBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$edit' } })
        },
      ),
    )
    const { findByLabelText, getByLabelText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    const panel = await findByLabelText('Thread')
    await within(panel).findByText('thread typo')

    fireEvent.click(await within(panel).findByRole('button', { name: 'Edit' }))
    const textarea = getByLabelText('Reply in thread') as HTMLTextAreaElement
    expect(textarea.value).toBe('thread typo')
    fireEvent.input(textarea, { target: { value: 'thread fix' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(editBody.body).toBe('thread fix'))
    expect(editTarget).toBe('$mine')
  })

  it('deletes an own message from the thread pane after confirmation', async () => {
    let deletedTarget: string | null = null
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                event('$mine', 200, {
                  sender: OWN_USER,
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'remove me',
                }),
              ],
              next_cursor: null,
            },
          }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({
          data: event('$mine', 200, { sender: OWN_USER, redacted: true }),
        }),
      ),
      http.delete(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId`,
        ({ params }) => {
          deletedTarget = params.eventId as string
          return HttpResponse.json({ data: { event_id: '$redaction' } })
        },
      ),
    )
    const { findByLabelText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    const panel = await findByLabelText('Thread')
    await within(panel).findByText('remove me')

    fireEvent.click(
      await within(panel).findByRole('button', { name: 'Delete' }),
    )
    fireEvent.click(
      within(panel).getByRole('button', { name: 'Confirm delete' }),
    )

    await waitFor(() => expect(deletedTarget).toBe('$mine'))
  })

  it('sends media into the thread, not the room (ADR 0065)', async () => {
    let mediaBody: Record<string, unknown> = {}
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () => HttpResponse.json({ data: { events: [], next_cursor: null } }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$root', 100) }),
      ),
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/media/uploads`, () =>
        HttpResponse.json({
          data: { upload_id: '22222222-2222-4222-8222-222222222222' },
        }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send-media`,
        async ({ request }) => {
          mediaBody = (await request.json()) as Record<string, unknown>
          return HttpResponse.json({ data: { event_id: '$img' } })
        },
      ),
    )
    const { getByLabelText, findByLabelText, findByText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    const panel = await findByLabelText('Thread')

    fireEvent.drop(panel, {
      dataTransfer: {
        files: [new File(['bytes'], 'cat.png', { type: 'image/png' })],
        types: ['Files'],
      },
    })
    await findByText('cat.png')

    const textarea = getByLabelText('Reply in thread') as HTMLTextAreaElement
    fireEvent.keyDown(textarea, { key: 'Enter' })

    // The thread-scoped store supplies thread_root, so a file dropped in the
    // panel cannot leak into the room timeline.
    await waitFor(() => expect(mediaBody.thread_root).toBe('$root'))
    expect(mediaBody.caption).toBeNull()
  })

  it('an open thread panel appends a live reply frame (WCR-06)', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads/:rootId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                event('$m1', 200, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                  body: 'first reply',
                  content: { msgtype: 'm.text', body: 'first reply' },
                }),
              ],
              next_cursor: null,
            },
          }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$root', 100) }),
      ),
    )
    const { services, findByText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    expect(await findByText('first reply')).toBeTruthy()

    // A reply broadcast while the panel is open must appear without a
    // reload — the main timeline hides thread members, so the panel is the
    // only surface that can show it.
    services.live.start()
    services.sockets[0].emitOpen()
    services.sockets[0].emitMessage(
      JSON.stringify({
        type: 'timeline.event',
        account_id: ACCOUNT,
        payload: event('$m2', 300, {
          relates_to: { rel_type: 'm.thread', event_id: '$root' },
          body: 'live reply',
          content: { msgtype: 'm.text', body: 'live reply' },
        }),
      }),
    )
    expect(await findByText('live reply')).toBeTruthy()
  })
})

describe('keyboard shortcuts (ADR 0078)', () => {
  it('ArrowUp on an empty composer edits your last message', async () => {
    const { findByLabelText, findByText } = renderRoom([
      event('$theirs', 100),
      event('$mine', 200, { sender: OWN_USER, body: 'my last words' }),
      // Newer, but not ours — so it is not the one Up should pick.
      event('$later', 300),
    ])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.keyDown(textarea, { key: 'ArrowUp' })

    expect(await findByText('Editing')).toBeTruthy()
    const editing = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement
    expect(editing.value).toBe('my last words')
  })

  it('ArrowUp does nothing when the composer has text or a banner is up', async () => {
    const { findByLabelText, queryByText, findAllByRole } = renderRoom([
      event('$mine', 200, { sender: OWN_USER, body: 'my last words' }),
    ])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: 'half a thought' } })
    fireEvent.keyDown(textarea, { key: 'ArrowUp' })
    expect(queryByText('Editing')).toBeNull()
    expect(textarea.value).toBe('half a thought')

    // With a reply banner up, Up must not swap it for an edit.
    fireEvent.input(textarea, { target: { value: '' } })
    fireEvent.click((await findAllByRole('button', { name: 'Reply' }))[0])
    await findByLabelText('Message Ops')
    fireEvent.keyDown(
      (await findByLabelText('Message Ops')) as HTMLTextAreaElement,
      { key: 'ArrowUp' },
    )
    expect(queryByText('Editing')).toBeNull()
    expect(queryByText('Replying to')).toBeTruthy()
  })

  it('ArrowUp skips messages that cannot be edited', async () => {
    const { findByLabelText, findByText } = renderRoom([
      event('$old', 100, { sender: OWN_USER, body: 'editable one' }),
      event('$redacted', 200, { sender: OWN_USER, redacted: true }),
    ])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.keyDown(textarea, { key: 'ArrowUp' })

    expect(await findByText('Editing')).toBeTruthy()
    expect(
      ((await findByLabelText('Message Ops')) as HTMLTextAreaElement).value,
    ).toBe('editable one')
  })

  it('Escape closes the thread panel, then hands focus to the composer', async () => {
    const { services, findByLabelText, queryByLabelText } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    await findByLabelText('Thread')
    const before = services.composerFocus.value

    fireEvent.keyDown(document.body, { key: 'Escape' })

    await waitFor(() => expect(queryByLabelText('Thread')).toBeNull())
    expect(services.composerFocus.value).toBe(before + 1)
  })

  it('Escape cancelling a reply banner does not also close the thread', async () => {
    const {
      services,
      findByLabelText,
      findAllByRole,
      findByText,
      queryByText,
    } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    await findByLabelText('Thread')
    fireEvent.click((await findAllByRole('button', { name: 'Reply' }))[0])
    expect(await findByText('Replying to')).toBeTruthy()
    const before = services.composerFocus.value

    // The composer claims this Escape; the thread panel must survive it.
    fireEvent.keyDown(
      (await findByLabelText('Message Ops')) as HTMLTextAreaElement,
      { key: 'Escape' },
    )

    await waitFor(() => expect(queryByText('Replying to')).toBeNull())
    expect(await findByLabelText('Thread')).toBeTruthy()
    expect(services.composerFocus.value).toBe(before)
  })
})

describe('room command', () => {
  it('/room offers room-name completions and switches to the selected room', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: OWN_USER,
              room_id: ROOM,
              name: 'Ops',
              canonical_alias: null,
              topic: null,
              last_activity_ts: 0,
            },
            {
              account_id: ACCOUNT,
              account_user_id: OWN_USER,
              room_id: OTHER_ROOM,
              name: 'axontest',
              canonical_alias: '#axontest:bostoncoop.net',
              topic: null,
              last_activity_ts: 1,
            },
          ],
        }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/timeline`,
        () => HttpResponse.json({ data: { events: [], next_cursor: null } }),
      ),
    )
    const { findByLabelText, findByRole } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/room axont' } })

    const menu = await findByRole('listbox', { name: 'Room matches' })
    expect(menu.textContent).toContain('#axontest:bostoncoop.net')

    fireEvent.keyDown(textarea, { key: 'Enter' })
    expect(textarea.value).toBe('/room #axontest:bostoncoop.net ')

    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(OTHER_ROOM)}`,
      ),
    )

    // The keyed composer remounts for the new room; focus should follow it so
    // the user can keep typing without reaching for the mouse.
    const newComposer = (await findByLabelText(
      'Message axontest',
    )) as HTMLTextAreaElement
    await waitFor(() => expect(document.activeElement).toBe(newComposer))
  })

  it('/switch is a /room alias', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: OWN_USER,
              room_id: ROOM,
              name: 'Ops',
              canonical_alias: null,
              topic: null,
              last_activity_ts: 0,
            },
            {
              account_id: ACCOUNT,
              account_user_id: OWN_USER,
              room_id: OTHER_ROOM,
              name: 'axontest',
              canonical_alias: '#axontest:bostoncoop.net',
              topic: null,
              last_activity_ts: 1,
            },
          ],
        }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/timeline`,
        () => HttpResponse.json({ data: { events: [], next_cursor: null } }),
      ),
    )
    const { findByLabelText } = renderRoom(
      [event('$root', 100)],
      undefined,
      RoomsIndexStub,
    )
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/switch axontest' } })
    fireEvent.keyDown(textarea, { key: 'Escape' })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(OTHER_ROOM)}`,
      ),
    )
  })

  it('/search opens the URL-addressed search overlay with its args', async () => {
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/search deploy failed' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    // The overlay itself is ShellChrome's mount (SearchOverlay.test.tsx);
    // the command's contract is the URL param, cleared composer included.
    await waitFor(() =>
      expect(window.location.search).toBe('?search=deploy+failed'),
    )
    expect(textarea.value).toBe('')
  })

  it('answers a bare /room with its usage, not "unknown command"', async () => {
    const { findByLabelText, findByText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/room' } })
    fireEvent.keyDown(textarea, { key: 'Escape' }) // dismiss the completion menu
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(await findByText('usage: /room <room>')).toBeTruthy()
    // A known command used wrong keeps its text so it can be corrected.
    expect(textarea.value).toBe('/room')
  })

  it('/sort updates the persisted room-list sort', async () => {
    const { services, findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/sort oldest' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(services.settings.roomSort.value).toBe('oldest')
    expect(textarea.value).toBe('')
  })

  it('/pin and /unpin default to the current room', async () => {
    const { services, findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement
    const key = `${ACCOUNT}/${ROOM}`

    fireEvent.input(textarea, { target: { value: '/pin' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })
    expect(services.settings.pinnedRooms.value).toContain(key)

    fireEvent.input(textarea, { target: { value: '/unpin' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })
    expect(services.settings.pinnedRooms.value).not.toContain(key)
  })

  it('/refresh and /rooms refresh the room list', async () => {
    let roomRequests = 0
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () => {
        roomRequests += 1
        return HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: OWN_USER,
              room_id: ROOM,
              name: 'Ops',
              canonical_alias: null,
              topic: null,
              last_activity_ts: 0,
            },
          ],
        })
      }),
    )
    const { findByLabelText } = renderRoom([event('$root', 100)])
    await waitFor(() => expect(roomRequests).toBeGreaterThan(0))
    const mountedRequests = roomRequests
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/refresh' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })
    await waitFor(() => expect(roomRequests).toBeGreaterThan(mountedRequests))
    const refreshedRequests = roomRequests

    fireEvent.input(textarea, { target: { value: '/rooms' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })
    await waitFor(() => expect(roomRequests).toBeGreaterThan(refreshedRequests))
  })

  it('/join posts the M19c mutation and routes to the joined room', async () => {
    const joinedRoom = '!joined:hs'
    let joinBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: joinedRoom } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: OWN_USER,
              room_id: joinedRoom,
              name: 'Joined',
              canonical_alias: null,
              topic: null,
              last_activity_ts: 0,
            },
          ],
        }),
      ),
    )
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/join #joined:hs' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(joinedRoom)}`,
      ),
    )
    expect(joinBody).toEqual({
      room_id_or_alias: '#joined:hs',
      server_names: [],
    })
  })

  it.each([
    ['/join joined:hs', '#joined:hs'],
    ['/join joined@hs', '#joined:hs'],
    ['/join #joined', '#joined:hs'],
  ])('/join accepts shorthand alias %s', async (command, roomIdOrAlias) => {
    const joinedRoom = '!joined:hs'
    let joinBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: joinedRoom } })
        },
      ),
    )
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText(
      command === '/join #joined' ? 'Message Ops' : 'Message',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: command } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() =>
      expect(joinBody).toEqual({
        room_id_or_alias: roomIdOrAlias,
        server_names: [],
      }),
    )
  })

  it('/join shows a pending status while the request is in flight', async () => {
    const joinedRoom = '!joined:hs'
    let joinStarted = false
    let resolveJoin: (response: Response) => void = () => {}
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`, () => {
        joinStarted = true
        return new Promise<Response>((resolve) => {
          resolveJoin = resolve
        })
      }),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: OWN_USER,
              room_id: ROOM,
              name: 'Ops',
              canonical_alias: '#ops:hs',
              topic: null,
              last_activity_ts: 1,
            },
          ],
        }),
      ),
    )
    const { findByLabelText, findByRole, queryByRole } = renderRoom([
      event('$root', 100),
    ])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/join #joined:hs' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    const status = await findByRole('status')
    expect(status.textContent).toBe('Joining #joined:hs…')
    expect(status.closest('.composer')).not.toBeNull()
    await waitFor(() => expect(joinStarted).toBe(true))

    resolveJoin(HttpResponse.json({ data: { room_id: joinedRoom } }))
    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(joinedRoom)}`,
      ),
    )
    await waitFor(() => expect(queryByRole('status')).toBeNull())
  })

  it('/join accepts a Matrix.to event link with via hints', async () => {
    const joinedRoom = '!joined:hs'
    let joinBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: joinedRoom } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: OWN_USER,
              room_id: ROOM,
              name: 'Ops',
              canonical_alias: '#ops:hs',
              topic: null,
              last_activity_ts: 1,
            },
          ],
        }),
      ),
    )
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, {
      target: {
        value:
          '/join https://matrix.to/#/%23joined%3Ahs/%24event?via=hs&via=backup',
      },
    })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() =>
      expect(window.location.href).toContain(
        `/${ACCOUNT}/rooms/${encodeURIComponent(joinedRoom)}?event=%24event`,
      ),
    )
    expect(joinBody).toEqual({
      room_id_or_alias: '#joined:hs',
      server_names: ['hs', 'backup'],
    })
  })

  it('/join without a target opens the find/join room interface', async () => {
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/join' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() =>
      expect(window.location.pathname).toBe('/rooms/discover'),
    )
  })

  it('/find opens the room directory search interface', async () => {
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/find' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => {
      expect(window.location.pathname).toBe('/rooms/discover')
      expect(window.location.hash).toBe('#find')
    })
  })

  it('/create opens the create-room interface', async () => {
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/create' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => {
      expect(window.location.pathname).toBe('/rooms/create')
      expect(window.location.hash).toBe('#create')
    })
  })

  it('/create with arguments answers with usage', async () => {
    const { findByLabelText, findByText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/create extra' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(await findByText('usage: /create')).toBeTruthy()
    expect(textarea.value).toBe('/create extra')
  })

  it('/dm without a target opens the direct-message form', async () => {
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/dm' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(window.location.pathname).toBe('/rooms/dm'))
  })

  it('/invite sends normalized Matrix user IDs to the current room', async () => {
    const inviteBodies: unknown[] = []
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/invite`,
        async ({ request }) => {
          inviteBodies.push(await request.json())
          return HttpResponse.json({ data: {} })
        },
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/members`,
        () =>
          HttpResponse.json({
            data: [
              {
                user_id: OWN_USER,
                membership: 'join',
                display_name: 'Me',
                avatar_url: null,
              },
              {
                user_id: '@alice:hs',
                membership: 'invite',
                display_name: 'Alice',
                avatar_url: null,
              },
              {
                user_id: '@bob:Example.ORG',
                membership: 'invite',
                display_name: 'Bob',
                avatar_url: null,
              },
            ],
          }),
      ),
    )
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, {
      target: { value: '/invite Alice Bob:Example.ORG' },
    })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() =>
      expect(inviteBodies).toEqual([
        { user_id: '@alice:hs' },
        { user_id: '@bob:Example.ORG' },
      ]),
    )
    expect(textarea.value).toBe('')
  })

  it('/invite leaves the command recoverable when the server rejects it', async () => {
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/invite`,
        () =>
          HttpResponse.json(
            {
              error: {
                code: 'forbidden',
                message: 'invite blocked',
              },
            },
            { status: 403 },
          ),
      ),
    )
    const { findByLabelText, findByText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/invite bob' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(
      await findByText(
        'Could not invite users. This account is not allowed to invite people to this room.',
      ),
    ).toBeTruthy()
    expect(textarea.value).toBe('/invite bob')
  })

  it('/dm starts a direct message by localpart and routes to the room', async () => {
    const createdRoom = '!dm:hs'
    let dmBody: unknown = null
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`,
        async ({ request }) => {
          dmBody = await request.json()
          return HttpResponse.json({ data: { room_id: createdRoom } })
        },
      ),
    )
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/dm bob' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(dmBody).toEqual({ user_id: '@bob:hs' }))
    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(createdRoom)}`,
      ),
    )
  })

  it('/dm falls back to the room server when room metadata is missing', async () => {
    const roomWithServer = '!unknown:bostoncoop.net'
    let dmBody: unknown = null
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`,
        async ({ request }) => {
          dmBody = await request.json()
          return HttpResponse.json({ data: { room_id: '!dm:bostoncoop.net' } })
        },
      ),
    )
    const { findByLabelText } = renderRoom(
      [],
      `/${ACCOUNT}/rooms/${encodeURIComponent(roomWithServer)}`,
    )
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/dm bob' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() =>
      expect(dmBody).toEqual({ user_id: '@bob:bostoncoop.net' }),
    )
  })

  it('/dm repairs a full Matrix username missing @', async () => {
    const createdRoom = '!dm:hs'
    let dmBody: unknown = null
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`,
        async ({ request }) => {
          dmBody = await request.json()
          return HttpResponse.json({ data: { room_id: createdRoom } })
        },
      ),
    )
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/dm Bob:Example.ORG' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(dmBody).toEqual({ user_id: '@bob:Example.ORG' }))
  })

  it('/join repairs a bare alias using the account homeserver', async () => {
    let joinBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`,
        async ({ request }) => {
          joinBody = await request.json()
          return HttpResponse.json({ data: { room_id: '!ops:hs' } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/join ops' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() =>
      expect(joinBody).toEqual({
        room_id_or_alias: '#ops:hs',
        server_names: [],
      }),
    )
  })

  it('/join leaves the command recoverable when the server rejects it', async () => {
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`, () =>
        HttpResponse.json(
          {
            error: {
              code: 'forbidden',
              message: 'not invited',
            },
          },
          { status: 403 },
        ),
      ),
    )
    const { findByLabelText, findByText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/join #private:hs' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(await findByText('not invited')).toBeTruthy()
    expect(textarea.value).toBe('/join #private:hs')
    expect(window.location.pathname).toBe(
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
  })

  it('/join explains ambiguous room-entry timeouts', async () => {
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`, () =>
        HttpResponse.json(
          {
            error: {
              code: 'bad_gateway',
              message: 'join timed out after 30s',
            },
          },
          { status: 502 },
        ),
      ),
    )
    const { findByLabelText, findByText, queryByRole } = renderRoom([
      event('$root', 100),
    ])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/join #matrix:matrix.org' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(
      await findByText(
        /The room may still appear after sync catches up; for large federated rooms/,
      ),
    ).toBeTruthy()
    expect(queryByRole('status')).toBeNull()
    expect(textarea.value).toBe('/join #matrix:matrix.org')
  })

  it('/knock posts the M19c mutation with a reason and stays in place', async () => {
    let knockBody: unknown = null
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/knock`,
        async ({ request }) => {
          knockBody = await request.json()
          return HttpResponse.json({ data: { room_id: '!private:hs' } })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText('Message')) as HTMLTextAreaElement

    fireEvent.input(textarea, {
      target: { value: '/knock #private:hs please let me in' },
    })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() =>
      expect(knockBody).toEqual({
        room_id_or_alias: '#private:hs',
        reason: 'please let me in',
        server_names: [],
      }),
    )
    expect(textarea.value).toBe('')
    expect(window.location.pathname).toBe(
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
  })

  it('/leave posts the M19b mutation, refreshes rooms, and leaves the room route', async () => {
    let leaveRequests = 0
    let roomRequests = 0
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () => {
        roomRequests += 1
        return HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: OWN_USER,
              room_id: ROOM,
              name: 'Ops',
              canonical_alias: null,
              topic: null,
              last_activity_ts: 0,
            },
          ],
        })
      }),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/leave`,
        () => {
          leaveRequests += 1
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { services, findByLabelText } = renderRoom([event('$root', 100)])
    await waitFor(() => expect(roomRequests).toBeGreaterThan(0))
    const mountedRequests = roomRequests
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/leave' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(leaveRequests).toBe(1))
    await waitFor(() => expect(roomRequests).toBeGreaterThan(mountedRequests))
    await waitFor(() => expect(services.rooms.rooms.value).toEqual([]))
    await waitFor(() => expect(window.location.pathname).toBe('/'))
  })

  it('/leave asks for confirmation when this account is the only joined member', async () => {
    let leaveRequests = 0
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(false)
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/members`,
        () =>
          HttpResponse.json({
            data: [
              {
                user_id: OWN_USER,
                membership: 'join',
                display_name: 'Me',
              },
            ],
          }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/leave`,
        () => {
          leaveRequests += 1
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { findByLabelText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/leave' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(confirm).toHaveBeenCalledTimes(1))
    expect(confirm).toHaveBeenCalledWith(
      expect.stringContaining('only joined member'),
    )
    expect(leaveRequests).toBe(0)
    expect(textarea.value).toBe('/leave')
    expect(window.location.pathname).toBe(
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
    confirm.mockRestore()
  })

  it('/leave skips the last-member confirmation when another user is joined', async () => {
    let leaveRequests = 0
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(false)
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/members`,
        () =>
          HttpResponse.json({
            data: [
              {
                user_id: OWN_USER,
                membership: 'join',
                display_name: 'Me',
              },
              {
                user_id: '@bob:hs',
                membership: 'join',
                display_name: 'Bob',
              },
            ],
          }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/leave`,
        () => {
          leaveRequests += 1
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { findByLabelText } = renderRoom(
      [event('$root', 100)],
      undefined,
      RoomsIndexStub,
    )
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/leave' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(leaveRequests).toBe(1))
    expect(confirm).not.toHaveBeenCalled()
    confirm.mockRestore()
  })

  it('/part is a /leave alias', async () => {
    let leaveRequests = 0
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/leave`,
        () => {
          leaveRequests += 1
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { findByLabelText } = renderRoom(
      [event('$root', 100)],
      undefined,
      RoomsIndexStub,
    )
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/part' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(leaveRequests).toBe(1))
  })

  it('/forget leaves the command in place when the server rejects it', async () => {
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/forget`,
        () =>
          HttpResponse.json(
            {
              error: {
                code: 'bad_request',
                message: "room isn't left or banned",
              },
            },
            { status: 400 },
          ),
      ),
    )
    const { findByLabelText, findByText } = renderRoom([event('$root', 100)])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/forget' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(await findByText("room isn't left or banned")).toBeTruthy()
    expect(textarea.value).toBe('/forget')
    expect(window.location.pathname).toBe(
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
  })
})

describe('staged Escape when focus has drifted (ADR 0078)', () => {
  it('cancels a reply banner even when the composer is not focused', async () => {
    const { findByLabelText, findAllByRole, findByText, queryByText } =
      renderRoom([event('$1', 100)])
    await findByLabelText('Message Ops')
    fireEvent.click((await findAllByRole('button', { name: 'Reply' }))[0])
    expect(await findByText('Replying to')).toBeTruthy()

    // Focus lives on <body> for a beat while the composer remounts; an Escape
    // there must still reach the banner rather than falling through.
    ;(document.activeElement as HTMLElement | null)?.blur()
    fireEvent.keyDown(document.body, { key: 'Escape' })

    await waitFor(() => expect(queryByText('Replying to')).toBeNull())
  })

  it('cancels the banner before closing the thread panel', async () => {
    const {
      findByLabelText,
      findAllByRole,
      findByText,
      queryByLabelText,
      queryByText,
    } = renderRoom(
      [event('$root', 100)],
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?thread=%24root`,
    )
    await findByLabelText('Thread')
    fireEvent.click((await findAllByRole('button', { name: 'Reply' }))[0])
    expect(await findByText('Replying to')).toBeTruthy()
    ;(document.activeElement as HTMLElement | null)?.blur()

    fireEvent.keyDown(document.body, { key: 'Escape' })
    await waitFor(() => expect(queryByText('Replying to')).toBeNull())
    // One Escape, one stage: the thread survives.
    expect(await findByLabelText('Thread')).toBeTruthy()

    fireEvent.keyDown(document.body, { key: 'Escape' })
    await waitFor(() => expect(queryByLabelText('Thread')).toBeNull())
  })
})

describe('sending media (M-W8.5, ADR 0065)', () => {
  const UPLOAD_ID = '22222222-2222-4222-8222-222222222222'
  const UPLOAD_PATH = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/media/uploads`
  const SEND_MEDIA_PATH = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/send-media`

  function serveMediaSend(): { sendBody: () => Record<string, unknown> } {
    let sendBody: Record<string, unknown> = {}
    server.use(
      http.post(UPLOAD_PATH, () =>
        HttpResponse.json({ data: { upload_id: UPLOAD_ID } }),
      ),
      http.post(SEND_MEDIA_PATH, async ({ request }) => {
        sendBody = (await request.json()) as Record<string, unknown>
        return HttpResponse.json({ data: { event_id: '$img' } })
      }),
    )
    return { sendBody: () => sendBody }
  }

  const png = () => new File(['bytes'], 'cat.png', { type: 'image/png' })

  it('uploads a dropped file and sends it with the typed text as the caption', async () => {
    const seen = serveMediaSend()
    const { getByRole, getByLabelText, getByText, container } = renderRoom([
      event('$a', 100),
    ])
    await waitFor(() => getByText('body of $a'))

    const stream = container.querySelector('.room-stream')!
    fireEvent.drop(stream, {
      dataTransfer: { files: [png()], types: ['Files'] },
    })

    // Staged, not sent: the pause is what lets a caption be typed.
    await waitFor(() => getByText('cat.png'))
    expect(seen.sendBody()).toEqual({})

    const textarea = getByRole('textbox')
    fireEvent.input(textarea, { target: { value: 'look at this' } })
    fireEvent.submit(textarea.closest('form')!)

    await waitFor(() =>
      expect(seen.sendBody()).toEqual({
        upload_id: UPLOAD_ID,
        caption: 'look at this',
        reply_to: null,
        thread_root: null,
      }),
    )
    // The chip is gone once the send is under way.
    expect(getByLabelText('Attach a file')).toBeTruthy()
  })

  it('sends a staged file bare, with no caption, on an empty composer', async () => {
    const seen = serveMediaSend()
    const { getByRole, getByText, container } = renderRoom([event('$a', 100)])
    await waitFor(() => getByText('body of $a'))

    const stream = container.querySelector('.room-stream')!
    fireEvent.drop(stream, {
      dataTransfer: { files: [png()], types: ['Files'] },
    })
    await waitFor(() => getByText('cat.png'))

    fireEvent.submit(getByRole('textbox').closest('form')!)

    await waitFor(() =>
      expect(seen.sendBody()).toMatchObject({
        upload_id: UPLOAD_ID,
        caption: null,
      }),
    )
  })

  it('keeps a staged file for the room you staged it in (issue #89)', async () => {
    // The composer's text draft has always survived a room switch (ADR 0048);
    // the file did not, and it is the half that cannot be retyped.
    const { getByText, queryByText, container } = renderRoom([event('$a', 100)])
    await waitFor(() => getByText('body of $a'))
    fireEvent.drop(container.querySelector('.room-stream')!, {
      dataTransfer: { files: [png()], types: ['Files'] },
    })
    await waitFor(() => getByText('cat.png'))

    const go = (roomId: string) => {
      window.history.pushState(
        null,
        '',
        `/${ACCOUNT}/rooms/${encodeURIComponent(roomId)}`,
      )
      window.dispatchEvent(new PopStateEvent('popstate'))
    }

    go('!other:hs')

    // Not in the other room: `scope` is what keeps a send from picking up a
    // file staged somewhere else, and it is resolved during render, so there is
    // no frame in which this room could submit it.
    await waitFor(() => expect(queryByText('cat.png')).toBeNull())

    go(ROOM)

    expect(await waitFor(() => getByText('cat.png'))).toBeTruthy()
  })

  it('keeps a staged file across the mobile back-to-list route', async () => {
    // How a phone changes rooms: there is no sidebar to click, so the trip is
    // room -> `/` -> room, and `/` unmounts RoomPage entirely. Retention that
    // lives inside the component dies here while surviving the desktop
    // room-to-room switch — which is exactly how this reached a device.
    // A default route that is *not* RoomPage, so `/` unmounts it as the shell
    // does (`app.tsx` routes `/` to RoomsIndex).
    const { getByText, queryByText, container } = renderRoom(
      [event('$a', 100)],
      undefined,
      () => <p>room list</p>,
    )
    await waitFor(() => getByText('body of $a'))
    fireEvent.drop(container.querySelector('.room-stream')!, {
      dataTransfer: { files: [png()], types: ['Files'] },
    })
    await waitFor(() => getByText('cat.png'))

    const go = (path: string) => {
      window.history.pushState(null, '', path)
      window.dispatchEvent(new PopStateEvent('popstate'))
    }

    go('/')
    await waitFor(() => getByText('room list'))
    await waitFor(() => expect(queryByText('cat.png')).toBeNull())
    go(`/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`)

    expect(await waitFor(() => getByText('cat.png'))).toBeTruthy()
  })
})
