import {
  cleanup,
  fireEvent,
  render,
  waitFor,
  within,
} from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { LocationProvider, Route, Router } from 'preact-iso'
import { useCallback, useMemo, useState } from 'preact/hooks'
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
import { ShellActionsContext } from '../shell-actions'
import type { MemberDto, RoomDto } from '../stores/room-list'
import type { EventDto } from '../stores/timeline'
import { RECEIPT_DEBOUNCE_MS } from '../stores/ephemeral-sender'
import { memoryStorage } from '../test/memory-storage'
import { TEST_BASE_URL, testServices } from '../test/services'
import { RoomPage } from './RoomPage'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const TIMELINE_PATH = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/timeline`

// `relates_to` and `content` are free-form objects in the contract
// (generated as `Record<string, never>`), so overrides take them loosely
// and cast.
function event(
  id: string,
  ts: number,
  overrides: Partial<
    Omit<EventDto, 'relates_to' | 'content' | 'prev_content'>
  > & {
    relates_to?: unknown
    content?: unknown
    // Generated as `Record<string, never>`, so real state JSON needs the same
    // escape hatch `content` already has.
    prev_content?: unknown
  } = {},
): EventDto {
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
  } as EventDto
}

function member(
  user_id: string,
  membership: string,
  display_name: string | null = null,
): MemberDto {
  return { user_id, membership, display_name, avatar_url: null } as MemberDto
}

const DAY = 86_400_000
const T0 = Date.UTC(2026, 5, 1, 12, 0, 0)

const server = setupServer(
  // Threads are refreshed on every room mount; empty by default.
  http.get(
    `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`,
    () => HttpResponse.json({ data: [] }),
  ),
  // Members are refreshed on every room mount; empty by default.
  http.get(
    `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/members`,
    () => HttpResponse.json({ data: [] }),
  ),
  // Drafts + read markers are hydrated on every room mount (M-W6 steps 5b/5c);
  // empty by default, and debounced writes are accepted.
  http.get(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () =>
    HttpResponse.json({ data: { namespace: 'drafts', entries: {} } }),
  ),
  http.put(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () =>
    HttpResponse.json({ data: { updated_at: '2026-06-01T12:00:00Z' } }),
  ),
  // Outbound read receipts + typing notices (ADR 0067/0068) fire from the same
  // read/compose choke points; accepted as no-ops by default.
  http.post(`${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/read`, () =>
    HttpResponse.json({ data: {} }),
  ),
  http.put(`${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/typing`, () =>
    HttpResponse.json({ data: {} }),
  ),
  http.post(`${TEST_BASE_URL}/v1/accounts/:accountId/utds/redecrypt`, () =>
    HttpResponse.json({
      data: {
        selected: 0,
        attempted: 0,
        decrypted: 0,
        still_pending: 0,
        timed_out: false,
      },
    }),
  ),
)
const originalScrollIntoView = Element.prototype.scrollIntoView
const originalResizeObserver = globalThis.ResizeObserver
let resizeObserverCallback: ResizeObserverCallback | null = null
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  vi.useRealTimers()
  vi.restoreAllMocks()
  if (originalScrollIntoView === undefined) {
    delete (Element.prototype as { scrollIntoView?: Element['scrollIntoView'] })
      .scrollIntoView
  } else {
    Element.prototype.scrollIntoView = originalScrollIntoView
  }
  globalThis.ResizeObserver = originalResizeObserver
  resizeObserverCallback = null
  window.history.replaceState(null, '', '/')
})
afterAll(() => server.close())

/** The routed page under test; Router's types want at least two children. */
function routedRoomPage(services: ReturnType<typeof testServices>) {
  return (
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <Router>
          <Route path="/:accountId/rooms/:roomId" component={RoomPage} />
          <Route default component={RoomPage} />
        </Router>
      </LocationProvider>
    </ServicesContext.Provider>
  )
}

function routedRoomPageWithAwayRoute(
  services: ReturnType<typeof testServices>,
) {
  const AwayPage = () => <main>Unrelated page</main>
  return (
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <Router>
          <Route path="/:accountId/rooms/:roomId" component={RoomPage} />
          <Route path="/away" component={AwayPage} />
          <Route default component={AwayPage} />
        </Router>
      </LocationProvider>
    </ServicesContext.Provider>
  )
}

function routedRoomPageWithJumpButton(
  services: ReturnType<typeof testServices>,
) {
  function Harness() {
    const [jumpAction, setJumpActionState] = useState<(() => void) | null>(null)
    const setJumpAction = useCallback((action: (() => void) | null) => {
      setJumpActionState(() => action)
    }, [])
    const shellActions = useMemo(
      () => ({
        jumpAction,
        setJumpAction,
        openUnreadThreads: () => {},
        roomTitle: null,
        roomInfoAction: null,
        setRoomChrome: () => {},
      }),
      [jumpAction, setJumpAction],
    )
    return (
      <ServicesContext.Provider value={services}>
        <ShellActionsContext.Provider value={shellActions}>
          <button
            type="button"
            disabled={jumpAction === null}
            onClick={() => jumpAction?.()}
          >
            Jump
          </button>
          <LocationProvider>
            <Router>
              <Route path="/:accountId/rooms/:roomId" component={RoomPage} />
              <Route default component={RoomPage} />
            </Router>
          </LocationProvider>
        </ShellActionsContext.Provider>
      </ServicesContext.Provider>
    )
  }
  return <Harness />
}

function renderRoom(
  events: EventDto[],
  options: {
    members?: MemberDto[] | (() => MemberDto[])
    nextCursor?: string | null
    rooms?: RoomDto[]
    roomsPending?: boolean
    roomId?: string
    storage?: Storage
    url?: string
  } = {},
) {
  const roomId = options.roomId ?? ROOM
  server.use(
    http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
      options.roomsPending
        ? new Promise<Response>(() => {})
        : HttpResponse.json({
            data: options.rooms ?? [
              {
                account_id: ACCOUNT,
                account_user_id: '@me:hs',
                room_id: ROOM,
                name: 'Ops',
                topic: 'Daily operations',
                avatar_url: 'mxc://hs/avatar',
                canonical_alias: '#ops:hs',
                last_activity_ts: T0,
                last_event_id: '$last',
              },
            ],
          }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(roomId)}/timeline`,
      () =>
        HttpResponse.json({
          data: { events, next_cursor: options.nextCursor ?? null },
        }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/members`,
      () =>
        HttpResponse.json({
          data:
            typeof options.members === 'function'
              ? options.members()
              : (options.members ?? []),
        }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/media/${ACCOUNT}/hs/avatar`,
      () =>
        new HttpResponse('avatar-bytes', {
          headers: { 'content-type': 'image/png' },
        }),
    ),
  )
  window.history.replaceState(
    null,
    '',
    options.url ?? `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
  )
  const services = testServices({ storage: options.storage })
  const utils = render(routedRoomPage(services))
  return { services, ...utils }
}

function setTimelineScrollGeometry(
  timeline: HTMLElement,
  geometry: { clientHeight: number; scrollHeight: () => number },
) {
  Object.defineProperty(timeline, 'clientHeight', {
    configurable: true,
    get: () => geometry.clientHeight,
  })
  Object.defineProperty(timeline, 'scrollHeight', {
    configurable: true,
    get: geometry.scrollHeight,
  })
}

function installResizeObserver(): void {
  globalThis.ResizeObserver = class ResizeObserver {
    constructor(callback: ResizeObserverCallback) {
      resizeObserverCallback = callback
    }
    observe() {}
    unobserve() {}
    disconnect() {}
  }
}

function triggerObservedResize(): void {
  resizeObserverCallback?.([], {} as ResizeObserver)
}

async function triggerObservedResizeFrame(): Promise<void> {
  triggerObservedResize()
  await new Promise((resolve) => requestAnimationFrame(resolve))
}

describe('RoomPage', () => {
  it('renders the timeline ascending with the room title and day separators', async () => {
    const { findByText, container } = renderRoom([
      event('$2', T0 + DAY),
      event('$1', T0),
    ])

    expect(await findByText('body of $1')).toBeTruthy()
    await waitFor(() =>
      expect(container.querySelector('h1')!.textContent).toBe('Ops'),
    )
    const bodies = [...container.querySelectorAll('.event-body')].map(
      (el) => el.textContent,
    )
    expect(bodies).toEqual(['body of $1', 'body of $2'])
    expect(container.querySelectorAll('.day-separator')).toHaveLength(2)
  })

  it('keeps only one message action bar open at a time', async () => {
    const { findByText, container } = renderRoom([
      event('$1', T0),
      event('$2', T0 + 1),
    ])
    await findByText('body of $1')
    const row1 = container.querySelector('li.event-row[data-event-id="$1"]')!
    const row2 = container.querySelector('li.event-row[data-event-id="$2"]')!

    fireEvent.click(row1.querySelector('.event-body')!)
    expect(row1.classList.contains('actions-open')).toBe(true)
    expect(row2.classList.contains('actions-open')).toBe(false)

    fireEvent.click(row2.querySelector('.event-body')!)
    expect(row1.classList.contains('actions-open')).toBe(false)
    expect(row2.classList.contains('actions-open')).toBe(true)
  })

  it('closes inspect when another row takes the action bar', async () => {
    const { services, findByText, findByRole, container } = renderRoom([
      event('$1', T0),
      event('$2', T0 + 1),
    ])
    services.settings.developerMode.value = true
    await findByText('body of $1')
    const row1 = container.querySelector<HTMLElement>(
      'li.event-row[data-event-id="$1"]',
    )!
    const row2 = container.querySelector<HTMLElement>(
      'li.event-row[data-event-id="$2"]',
    )!

    fireEvent.click(row1.querySelector('.event-body')!)
    fireEvent.click(within(row1).getByRole('button', { name: 'Inspect' }))
    expect(
      await findByRole('region', { name: 'Event diagnostics for $1' }),
    ).toBeTruthy()

    fireEvent.click(row2.querySelector('.event-body')!)
    await waitFor(() =>
      expect(
        container.querySelector('[aria-label="Event diagnostics for $1"]'),
      ).toBeNull(),
    )
    expect(row1.classList.contains('actions-open')).toBe(false)
  })

  it('opens the action bar from a touch tap, not only a mouse click', async () => {
    const { findByText, container } = renderRoom([event('$1', T0)])
    await findByText('body of $1')
    const row = container.querySelector('li.event-row[data-event-id="$1"]')!
    const body = row.querySelector('.event-body')!

    fireEvent.pointerDown(body, {
      pointerType: 'touch',
      pointerId: 1,
      clientX: 40,
      clientY: 40,
    })
    fireEvent.pointerUp(body, {
      pointerType: 'touch',
      pointerId: 1,
      clientX: 42,
      clientY: 41,
    })

    expect(row.classList.contains('actions-open')).toBe(true)
  })

  it('does not open the action bar when the touch was a scroll', async () => {
    const { findByText, container } = renderRoom([event('$1', T0)])
    await findByText('body of $1')
    const row = container.querySelector('li.event-row[data-event-id="$1"]')!
    const body = row.querySelector('.event-body')!

    fireEvent.pointerDown(body, {
      pointerType: 'touch',
      pointerId: 1,
      clientX: 40,
      clientY: 40,
    })
    fireEvent.pointerMove(body, {
      pointerType: 'touch',
      pointerId: 1,
      clientX: 40,
      clientY: 80,
    })
    fireEvent.pointerUp(body, {
      pointerType: 'touch',
      pointerId: 1,
      clientX: 40,
      clientY: 80,
    })

    expect(row.classList.contains('actions-open')).toBe(false)
  })

  it('does not expose the raw room id in the composer while the title loads', async () => {
    const { findByText, getByRole } = renderRoom([event('$1', T0)], {
      roomsPending: true,
    })

    await findByText('body of $1')
    const composer = getByRole('textbox', { name: 'Message' })

    expect((composer as HTMLTextAreaElement).placeholder).toBe('Message')
    expect((composer as HTMLTextAreaElement).placeholder).not.toContain(ROOM)
  })

  it('uses the cached room title while the room list loads', async () => {
    const storage = memoryStorage({
      'axon.token': 'tok-test',
      'axon.room_titles.v1': JSON.stringify({
        version: 1,
        titles: [[[ACCOUNT, ROOM].join('/'), 'Cached Ops']],
      }),
    })
    const { findByText, getByRole } = renderRoom([event('$1', T0)], {
      roomsPending: true,
      storage,
    })

    await findByText('body of $1')
    const composer = getByRole('textbox', { name: 'Message Cached Ops' })
    expect((composer as HTMLTextAreaElement).placeholder).toBe(
      'Message Cached Ops',
    )
  })

  it('opens room information from the title button', async () => {
    const { findByLabelText, findByRole } = renderRoom([event('$1', T0)], {
      members: [
        member('@left:hs', 'leave', 'Left User'),
        member('@alice:hs', 'join', 'Alice'),
        member('@carol:hs', 'invite', 'Carol'),
      ],
    })

    await findByLabelText('Message Ops')
    const titleButton = await findByRole('button', {
      name: 'Open room information',
    })
    fireEvent.click(titleButton)

    const panel = await findByRole('complementary', {
      name: 'Room information',
    })
    expect(panel.querySelector('.room-info-identity-name')?.textContent).toBe(
      'Ops',
    )
    expect(panel.querySelector('.room-info-identity-topic')?.textContent).toBe(
      'Daily operations',
    )
    await waitFor(() => {
      expect(
        panel.querySelector('.room-info-identity .room-avatar img'),
      ).toBeTruthy()
    })
    expect(within(panel).getAllByText('Daily operations').length).toBe(2)
    expect(within(panel).getByText('#ops:hs')).toBeTruthy()
    expect(
      within(panel).getAllByText('Unavailable from current API').length,
    ).toBeGreaterThan(0)
    expect(within(panel).getByText('Alice')).toBeTruthy()
  })

  it('shows joined parent spaces from their child relationships', async () => {
    const parentSpace = '!engineering:hs'
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/space/children`,
        ({ params }) =>
          HttpResponse.json({
            data:
              params.roomId === parentSpace
                ? [
                    {
                      room_id: ROOM,
                      name: 'Ops',
                      room_type: null,
                      suggested: false,
                      via: ['hs'],
                    },
                  ]
                : [],
          }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/space/parents`,
        () => HttpResponse.json({ data: [] }),
      ),
    )
    const { findByLabelText, findByRole } = renderRoom([event('$1', T0)], {
      rooms: [
        {
          account_id: ACCOUNT,
          account_user_id: '@me:hs',
          room_id: ROOM,
          name: 'Ops',
          last_activity_ts: T0,
          highlight_count: 0,
          notification_count: 0,
        },
        {
          account_id: ACCOUNT,
          account_user_id: '@me:hs',
          room_id: parentSpace,
          name: 'Engineering',
          room_type: 'm.space',
          last_activity_ts: T0 - 1,
          highlight_count: 0,
          notification_count: 0,
        },
      ],
    })

    await findByLabelText('Message Ops')
    fireEvent.click(
      await findByRole('button', { name: 'Open room information' }),
    )

    expect(
      await findByRole('button', { name: 'Parent: Engineering' }),
    ).toBeTruthy()
  })

  it('sorts and filters the room information member roster', async () => {
    const { container, findByLabelText, findByRole } = renderRoom(
      [event('$1', T0)],
      {
        members: [
          member('@zara:hs', 'leave', 'Zara'),
          member('@bob:hs', 'join', 'Bob'),
          member('@alice:hs', 'join', 'Alice'),
          member('@ivy:hs', 'invite', 'Ivy'),
        ],
      },
    )

    await findByLabelText('Message Ops')
    fireEvent.click(
      await findByRole('button', { name: 'Open room information' }),
    )

    const panel = await findByLabelText('Room information')
    await within(panel).findByText('Alice')
    expect(
      [...container.querySelectorAll('.member-row')].map(
        (row) => row.querySelector('.member-name')?.textContent,
      ),
    ).toEqual(['Alice', 'Bob', 'Ivy', 'Zara'])

    fireEvent.input(await findByLabelText('Filter members'), {
      target: { value: 'invite' },
    })

    await waitFor(() =>
      expect(
        [...container.querySelectorAll('.member-row')].map(
          (row) => row.querySelector('.member-name')?.textContent,
        ),
      ).toEqual(['Ivy']),
    )
  })

  it('refreshes members from the room information panel', async () => {
    let calls = 0
    const { findByLabelText, findByRole } = renderRoom([event('$1', T0)], {
      members: () => {
        calls += 1
        return calls === 1
          ? [member('@alice:hs', 'join', 'Alice')]
          : [
              member('@alice:hs', 'join', 'Alice'),
              member('@bob:hs', 'join', 'Bob'),
            ]
      },
    })

    await findByLabelText('Message Ops')
    fireEvent.click(
      await findByRole('button', { name: 'Open room information' }),
    )
    const panel = await findByLabelText('Room information')
    expect(await within(panel).findByText('Alice')).toBeTruthy()

    fireEvent.click(await findByRole('button', { name: 'Refresh' }))

    expect(await within(panel).findByText('Bob')).toBeTruthy()
  })

  it('opens an existing DM from a room information member row', async () => {
    const existingRoom = '!dm:hs'
    let dmCalls = 0
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(existingRoom)}/members`,
        () =>
          HttpResponse.json({
            data: [
              member('@me:hs', 'join', 'Me'),
              member('@alice:hs', 'join', 'Alice'),
            ],
          }),
      ),
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`, () => {
        dmCalls += 1
        return HttpResponse.json({ data: { room_id: '!new-dm:hs' } })
      }),
    )
    const { findByLabelText, findByRole, queryByLabelText } = renderRoom(
      [event('$1', T0)],
      {
        rooms: [
          {
            account_id: ACCOUNT,
            account_user_id: '@me:hs',
            room_id: ROOM,
            name: 'Ops',
            canonical_alias: '#ops:hs',
            topic: null,
            last_activity_ts: 10,
            notification_count: 0,
            highlight_count: 0,
          },
          {
            account_id: ACCOUNT,
            account_user_id: '@me:hs',
            room_id: existingRoom,
            name: null,
            canonical_alias: null,
            topic: null,
            last_activity_ts: 0,
            notification_count: 0,
            highlight_count: 0,
          },
        ],
        members: [
          member('@me:hs', 'join', 'Me'),
          member('@alice:hs', 'join', 'Alice'),
        ],
      },
    )

    await findByLabelText('Message Ops')
    fireEvent.click(
      await findByRole('button', { name: 'Open room information' }),
    )
    const panel = await findByLabelText('Room information')

    const aliceDm = within(panel).getByRole('button', {
      name: 'Open DM with Alice (@alice:hs)',
    })
    expect(aliceDm.getAttribute('title')).toBe('Open DM with Alice (@alice:hs)')
    expect(
      within(panel).queryByRole('button', {
        name: 'Open DM with Me (@me:hs)',
      }),
    ).toBeNull()
    expect(within(panel).getAllByText('Joined')).toHaveLength(2)
    const aliceAvatar = aliceDm.querySelector('.user-avatar')
    expect(aliceAvatar).toBeTruthy()
    fireEvent.click(aliceAvatar!)

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(existingRoom)}`,
      ),
    )
    await waitFor(() => expect(queryByLabelText('Room information')).toBeNull())
    expect(dmCalls).toBe(0)
  })

  it('keeps the room information panel open when member DM creation fails', async () => {
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/dm`, () =>
        HttpResponse.json(
          { error: { code: 'forbidden', message: 'blocked' } },
          { status: 403 },
        ),
      ),
    )
    const { findByLabelText, findByRole, findByText } = renderRoom(
      [event('$1', T0)],
      {
        members: [member('@alice:hs', 'join', 'Alice')],
      },
    )

    await findByLabelText('Message Ops')
    fireEvent.click(
      await findByRole('button', { name: 'Open room information' }),
    )
    const panel = await findByLabelText('Room information')
    fireEvent.click(
      within(panel).getByRole('button', {
        name: 'Open DM with Alice (@alice:hs)',
      }),
    )

    expect(await findByText(/Could not start DM with @alice:hs/)).toBeTruthy()
    expect(await findByLabelText('Room information')).toBeTruthy()
  })

  it('copies a Matrix.to room link from the room information panel', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    const { findByLabelText, findByRole } = renderRoom([event('$1', T0)])

    await findByLabelText('Message Ops')
    fireEvent.click(
      await findByRole('button', { name: 'Open room information' }),
    )
    const panel = await findByLabelText('Room information')
    fireEvent.click(within(panel).getByRole('button', { name: 'Copy link' }))

    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith('https://matrix.to/#/%23ops%3Ahs'),
    )
    expect((await within(panel).findByRole('status')).textContent).toBe(
      'Copied',
    )
  })

  it('copies a routable Matrix.to link for rooms without aliases', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    const roomId = '!Oha01wS8odafKACi1LVh_QLxBRJVaMYRCEbISYUJYKk'
    const { findByLabelText, findByRole } = renderRoom(
      [event('$1', T0, { room_id: roomId })],
      {
        roomId,
        url: `/${ACCOUNT}/rooms/${encodeURIComponent(roomId)}`,
        rooms: [
          {
            account_id: ACCOUNT,
            account_user_id: '@me:bostoncoop.net',
            room_id: roomId,
            name: 'Opaque room',
            topic: null,
            avatar_url: null,
            canonical_alias: null,
            last_activity_ts: T0,
            last_event_id: '$last',
          } as RoomDto,
        ],
      },
    )

    await findByLabelText('Message Opaque room')
    fireEvent.click(
      await findByRole('button', { name: 'Open room information' }),
    )
    const panel = await findByLabelText('Room information')
    fireEvent.click(within(panel).getByRole('button', { name: 'Copy link' }))

    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith(
        `https://matrix.to/#/${roomId}?via=bostoncoop.net`,
      ),
    )
  })

  it('invites normalized Matrix user IDs from the room information panel', async () => {
    const inviteBodies: unknown[] = []
    let membersAfterInvite = false
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/members`,
        () =>
          HttpResponse.json({
            data: membersAfterInvite
              ? [
                  member('@me:hs', 'join', 'Me'),
                  member('@alice:hs', 'invite', 'Alice'),
                  member('@bob:Example.NET', 'invite', 'Bob'),
                ]
              : [member('@me:hs', 'join', 'Me')],
          }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/invite`,
        async ({ request }) => {
          inviteBodies.push(await request.json())
          membersAfterInvite = true
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { findByLabelText, findByRole } = renderRoom([event('$1', T0)])

    await findByLabelText('Message Ops')
    fireEvent.click(
      await findByRole('button', { name: 'Open room information' }),
    )
    const panel = await findByLabelText('Room information')
    fireEvent.click(within(panel).getByRole('button', { name: 'Invite' }))
    const invite = await within(panel).findByLabelText('Invite people')
    fireEvent.input(invite, { target: { value: 'Alice Bob:Example.NET' } })
    fireEvent.click(within(panel).getByRole('button', { name: 'Send invite' }))

    await waitFor(() =>
      expect(inviteBodies).toEqual([
        { user_id: '@alice:hs' },
        { user_id: '@bob:Example.NET' },
      ]),
    )
    expect(
      await within(panel).findByText('Invited @alice:hs and @bob:Example.NET.'),
    ).toBeTruthy()
    expect((invite as HTMLInputElement).value).toBe('')
  })

  it('cancels a pending invite from the room information status button', async () => {
    const kickBodies: unknown[] = []
    let inviteCanceled = false
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/kick`,
        async ({ request }) => {
          kickBodies.push(await request.json())
          inviteCanceled = true
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { findByLabelText, findByRole, queryByRole } = renderRoom(
      [event('$1', T0)],
      {
        members: () =>
          inviteCanceled
            ? [member('@me:hs', 'join', 'Me')]
            : [
                member('@me:hs', 'join', 'Me'),
                member('@alice:hs', 'invite', 'Alice'),
              ],
      },
    )

    await findByLabelText('Message Ops')
    fireEvent.click(
      await findByRole('button', { name: 'Open room information' }),
    )
    const panel = await findByLabelText('Room information')
    const statusButton = await within(panel).findByRole('button', {
      name: 'Cancel invite for Alice (@alice:hs)',
    })
    expect(statusButton.textContent).toBe('Invited')

    fireEvent.click(statusButton)
    const dialog = await findByRole('dialog', {
      name: 'Cancel invite for Alice',
    })
    fireEvent.click(within(dialog).getByRole('button', { name: 'Keep invite' }))
    await waitFor(() =>
      expect(
        queryByRole('dialog', { name: 'Cancel invite for Alice' }),
      ).toBeNull(),
    )
    expect(kickBodies).toEqual([])

    fireEvent.click(statusButton)
    fireEvent.click(
      within(
        await findByRole('dialog', { name: 'Cancel invite for Alice' }),
      ).getByRole('button', { name: 'Cancel invite' }),
    )

    await waitFor(() => expect(kickBodies).toEqual([{ user_id: '@alice:hs' }]))
    expect(
      await within(panel).findByText('Canceled invite for Alice.'),
    ).toBeTruthy()
    await waitFor(() =>
      expect(
        within(panel).queryByRole('button', {
          name: 'Cancel invite for Alice (@alice:hs)',
        }),
      ).toBeNull(),
    )
  })

  it('confirms before leaving from the room information panel', async () => {
    let leaveCalls = 0
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/leave`,
        () => {
          leaveCalls += 1
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { findByLabelText, findByRole, queryByRole } = renderRoom(
      [event('$1', T0)],
      { members: [member('@me:hs', 'join', 'Me')] },
    )

    await findByLabelText('Message Ops')
    fireEvent.click(
      await findByRole('button', { name: 'Open room information' }),
    )
    const panel = await findByLabelText('Room information')
    fireEvent.click(within(panel).getByRole('button', { name: 'Leave' }))

    const dialog = await findByRole('dialog', { name: 'Leave Ops' })
    expect(
      within(dialog).getByText(/You are the only joined member in this room/),
    ).toBeTruthy()
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }))
    await waitFor(() =>
      expect(queryByRole('dialog', { name: 'Leave Ops' })).toBeNull(),
    )
    expect(leaveCalls).toBe(0)

    fireEvent.click(within(panel).getByRole('button', { name: 'Leave' }))
    fireEvent.click(
      within(await findByRole('dialog', { name: 'Leave Ops' })).getByRole(
        'button',
        { name: 'Leave room' },
      ),
    )

    await waitFor(() => expect(window.location.pathname).toBe('/'))
    expect(leaveCalls).toBe(1)
  })

  it('/whereami opens room information and clears the composer', async () => {
    const { findByLabelText, findByRole } = renderRoom([event('$1', T0)])
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/whereami' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(
      await findByRole('complementary', { name: 'Room information' }),
    ).toBeTruthy()
    expect(textarea.value).toBe('')
  })

  it('/cancel cancels a pending invite by normalized Matrix user ID', async () => {
    const kickBodies: unknown[] = []
    let inviteCanceled = false
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/kick`,
        async ({ request }) => {
          kickBodies.push(await request.json())
          inviteCanceled = true
          return HttpResponse.json({ data: {} })
        },
      ),
    )
    const { findByLabelText } = renderRoom([event('$1', T0)], {
      members: () =>
        inviteCanceled
          ? [member('@me:hs', 'join', 'Me')]
          : [
              member('@me:hs', 'join', 'Me'),
              member('@alice:hs', 'invite', 'Alice'),
            ],
    })
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/cancel Alice' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    await waitFor(() => expect(kickBodies).toEqual([{ user_id: '@alice:hs' }]))
    await waitFor(() => expect(textarea.value).toBe(''))
  })

  it('/cancel keeps the command recoverable when the user has no pending invite', async () => {
    const { findByLabelText, findByText } = renderRoom([event('$1', T0)], {
      members: [
        member('@me:hs', 'join', 'Me'),
        member('@alice:hs', 'join', 'Alice'),
      ],
    })
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement

    fireEvent.input(textarea, { target: { value: '/cancel Alice' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(await findByText('No pending invite for @alice:hs.')).toBeTruthy()
    expect(textarea.value).toBe('/cancel Alice')
  })

  it('shows placeholders for UTDs and redacted events', async () => {
    const { findByText } = renderRoom([
      event('$utd', T0, {
        type: 'm.room.encrypted',
        content: null,
        body: null,
      }),
      event('$gone', T0 + 1, {
        redacted: true,
        content: null,
        body: null,
        redaction_event_id: '$r',
      }),
    ])

    expect(await findByText('unable to decrypt')).toBeTruthy()
    expect(await findByText('message deleted')).toBeTruthy()
  })

  it('loads the destination timeline after clicking a room hyperlink', async () => {
    const roomA = '!room-a:hs'
    const roomB = '!room-b:hs'
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: roomA,
              canonical_alias: '#a:hs',
              name: 'Room A',
              last_activity_ts: T0,
            },
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: roomB,
              canonical_alias: '#b:hs',
              name: 'Room B',
              last_activity_ts: T0 + 1,
            },
          ],
        }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/timeline`,
        ({ params }) => {
          const roomId = decodeURIComponent(params.roomId as string)
          return HttpResponse.json({
            data: {
              events:
                roomId === roomB
                  ? [event('$b', T0 + 1, { room_id: roomB, body: 'body in B' })]
                  : [
                      event('$a', T0, {
                        room_id: roomA,
                        body: 'visit #b:hs',
                        content: {
                          msgtype: 'm.text',
                          body: 'visit #b:hs',
                          format: 'org.matrix.custom.html',
                          formatted_body:
                            'visit <a href="https://matrix.to/#/%23b%3Ahs">#b:hs</a>',
                        },
                      }),
                    ],
              next_cursor: null,
            },
          })
        },
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(roomA)}`,
    )
    const { findByRole, findByText, queryByText } = render(
      routedRoomPage(testServices()),
    )

    const link = await findByRole('link', { name: '#b:hs' })
    fireEvent.click(link)

    expect(await findByText('body in B')).toBeTruthy()
    expect(queryByText(/No messages loaded|No displayable events/)).toBeNull()
  })

  it('jumps to the destination event after clicking a Matrix.to room-event hyperlink', async () => {
    const roomA = '!room-a:hs'
    const roomB = '!room-b:hs'
    const targetEvent = '$K0l0ndHCq0wBKulL5FdPylfvfVHiMomYv6gEtNZf-2E'
    const targetTs = T0 - 5 * DAY
    let seenAtTs: string | null = null
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: roomA,
              canonical_alias: '#a:hs',
              name: 'Room A',
              last_activity_ts: T0,
            },
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: roomB,
              canonical_alias: '#b:hs',
              name: 'Room B',
              last_activity_ts: T0 + 1,
            },
          ],
        }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        ({ params }) =>
          HttpResponse.json({
            data: event(params.eventId as string, targetTs, {
              room_id: roomB,
              body: 'target body',
            }),
          }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/timeline`,
        ({ params, request }) => {
          const roomId = decodeURIComponent(params.roomId as string)
          seenAtTs = new URL(request.url).searchParams.get('at_ts')
          return HttpResponse.json({
            data: {
              events:
                roomId === roomB
                  ? [
                      event(targetEvent, targetTs, {
                        room_id: roomB,
                        body: 'target body',
                      }),
                    ]
                  : [
                      event('$a', T0, {
                        room_id: roomA,
                        body: 'visit linked event',
                        content: {
                          msgtype: 'm.text',
                          body: 'visit linked event',
                          format: 'org.matrix.custom.html',
                          formatted_body: `visit <a href="https://matrix.to/#/${encodeURIComponent(roomB)}/${encodeURIComponent(targetEvent)}?via=hs">linked event</a>`,
                        },
                      }),
                    ],
              next_cursor: null,
            },
          })
        },
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(roomA)}`,
    )
    const { findByRole, findByText, container } = render(
      routedRoomPage(testServices()),
    )

    fireEvent.click(await findByRole('link', { name: 'linked event' }))

    expect(await findByText('target body')).toBeTruthy()
    expect(seenAtTs).toBe(String(targetTs))
    await waitFor(() =>
      expect(
        container
          .querySelector('.event-row.highlighted')
          ?.getAttribute('data-event-id'),
      ).toBe(targetEvent),
    )
  })

  it('hides redacted events when the setting is on', async () => {
    const { services, findByText, queryByText, container } = renderRoom([
      event('$kept', T0, { body: 'still here' }),
      event('$gone', T0 + 1, {
        redacted: true,
        content: null,
        body: null,
        redaction_event_id: '$r',
      }),
    ])

    expect(await findByText('still here')).toBeTruthy()
    expect(await findByText('message deleted')).toBeTruthy()

    services.settings.hideRedactedEvents.value = true
    await waitFor(() => expect(queryByText('message deleted')).toBeNull())
    expect(container.querySelector('[data-event-id="$gone"]')).toBeNull()
    expect(await findByText('still here')).toBeTruthy()
  })

  it('retries UTD decryption once and reloads when rows decrypt', async () => {
    let timelineCalls = 0
    let redecryptCalls = 0
    server.use(
      http.get(TIMELINE_PATH, () => {
        timelineCalls += 1
        return HttpResponse.json({
          data: {
            events:
              timelineCalls === 1
                ? [
                    event('$utd', T0, {
                      type: 'm.room.encrypted',
                      content: null,
                      body: null,
                    }),
                  ]
                : [event('$utd', T0, { body: 'decrypted body' })],
            next_cursor: null,
          },
        })
      }),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/utds/redecrypt`,
        () => {
          redecryptCalls += 1
          return HttpResponse.json({
            data: {
              selected: 1,
              attempted: 1,
              decrypted: 1,
              still_pending: 0,
              timed_out: false,
            },
          })
        },
      ),
    )

    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: ROOM,
              name: 'Ops',
              last_activity_ts: T0,
            },
          ],
        }),
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
    const { findByText } = render(routedRoomPage(testServices()))

    expect(await findByText('decrypted body')).toBeTruthy()
    expect(redecryptCalls).toBe(1)
    expect(timelineCalls).toBe(2)
  })

  it('hides unsupported bodyless events outside developer mode', async () => {
    const { findByText, queryByText, container } = renderRoom([
      event('$call', T0, {
        type: 'm.call.invite',
        body: null,
        content: { call_id: 'call-1' },
      }),
    ])

    expect(await findByText(/No displayable events on this page/)).toBeTruthy()
    expect(queryByText('unsupported event: m.call.invite')).toBeNull()
    expect(container.querySelector('[data-event-id="$call"]')).toBeNull()
  })

  it('explains that an empty room timeline may still be syncing', async () => {
    const { findByText } = renderRoom([])

    expect(
      await findByText(/Newly joined large rooms can take a little while/),
    ).toBeTruthy()
  })

  it('shows per-event diagnostics only in developer mode', async () => {
    const { services, findByText, findByRole, queryByRole, queryByText } =
      renderRoom([
        event('$debug', T0, {
          type: 'm.call.invite',
          body: null,
          content: { call_id: 'call-1' },
          sender_trust: 'verified',
        }),
      ])

    expect(await findByText(/No displayable events on this page/)).toBeTruthy()
    expect(queryByText('unsupported event: m.call.invite')).toBeNull()
    expect(queryByRole('button', { name: 'Inspect' })).toBeNull()

    services.settings.developerMode.value = true
    expect(await findByText('unsupported event: m.call.invite')).toBeTruthy()
    fireEvent.click(await findByRole('button', { name: 'Inspect' }))

    const inspector = await findByRole('region', {
      name: 'Event diagnostics for $debug',
    })
    expect(inspector.textContent).toContain('"event_id": "$debug"')
    expect(inspector.textContent).toContain('"type": "m.call.invite"')
    expect(inspector.textContent).toContain('"call_id": "call-1"')
    expect(inspector.textContent).toContain('"sender_trust": "verified"')
  })

  it('copies inspect JSON from the title-bar clipboard control', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    vi.stubGlobal('navigator', { clipboard: { writeText } })
    const { services, findByText, findByRole } = renderRoom([
      event('$debug', T0, {
        type: 'm.call.invite',
        body: null,
        content: { call_id: 'call-1' },
        sender_trust: 'verified',
      }),
    ])

    services.settings.developerMode.value = true
    expect(await findByText('unsupported event: m.call.invite')).toBeTruthy()
    fireEvent.click(await findByRole('button', { name: 'Inspect' }))
    const inspector = await findByRole('region', {
      name: 'Event diagnostics for $debug',
    })
    fireEvent.click(await findByRole('button', { name: 'Copy API event data' }))

    await waitFor(() => expect(writeText).toHaveBeenCalledOnce())
    expect(inspector.querySelector('[role="status"]')?.textContent).toBe(
      'Copied',
    )
    const payload = JSON.parse(writeText.mock.calls[0][0] as string) as {
      event_id: string
      type: string
      content: { call_id: string }
    }
    expect(payload.event_id).toBe('$debug')
    expect(payload.type).toBe('m.call.invite')
    expect(payload.content.call_id).toBe('call-1')
    vi.unstubAllGlobals()
  })

  it('tiers state events by the persisted visibility setting', async () => {
    // The toggle lives in Settings now, so the room reads the persisted
    // preference rather than owning an ephemeral checkbox.
    const { services, findByText, queryByText } = renderRoom([
      event('$msg', T0),
      event('$member', T0 + 1, {
        type: 'm.room.member',
        state_key: '@bob:hs',
        content: { membership: 'join' },
        body: null,
      }),
      event('$topic', T0 + 2, {
        type: 'm.room.topic',
        state_key: '',
        content: { topic: 'new topic' },
        body: null,
      }),
    ])

    // The default tier: the membership notice shows, room config does not.
    expect(await findByText('body of $msg')).toBeTruthy()
    expect(await findByText('@bob joined the room')).toBeTruthy()
    expect(queryByText(/m\.room\.topic/)).toBeNull()

    services.settings.stateEvents.value = 'all'
    expect(await findByText(/m\.room\.topic/)).toBeTruthy()

    services.settings.stateEvents.value = 'hidden'
    await waitFor(() => {
      expect(queryByText(/m\.room\.topic/)).toBeNull()
      expect(queryByText('@bob joined the room')).toBeNull()
    })
  })

  it('renders a display-name change as such, not as a join (issue #31)', async () => {
    const { findByText, queryByText } = renderRoom([
      event('$rename', T0, {
        type: 'm.room.member',
        state_key: '@bob:hs',
        sender: '@bob:hs',
        content: { membership: 'join', displayname: 'Bob B' },
        prev_content: { membership: 'join', displayname: 'Bob' },
        body: null,
      }),
    ])

    expect(
      await findByText('Bob changed their display name to Bob B'),
    ).toBeTruthy()
    expect(queryByText(/joined the room/)).toBeNull()
  })

  it('no longer offers a state-events checkbox in the room header', async () => {
    const { findByText, queryByLabelText } = renderRoom([event('$msg', T0)])
    await findByText('body of $msg')

    expect(queryByLabelText('State events')).toBeNull()
    expect(queryByLabelText('Jump to date')).toBeNull()
  })

  it('jumps to the selected local calendar day from the shell action', async () => {
    let seenAtTs: string | null = null
    const expectedStartTs = new Date(2026, 4, 20).getTime()
    const expectedAtTs = new Date(2026, 4, 21).getTime() - 1 // end of local 2026-05-20
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: ROOM,
              name: 'Ops',
              topic: null,
              avatar_url: null,
              canonical_alias: null,
              last_activity_ts: T0,
              last_event_id: '$latest',
            },
          ],
        }),
      ),
      http.get(TIMELINE_PATH, ({ request }) => {
        seenAtTs = new URL(request.url).searchParams.get('at_ts')
        return HttpResponse.json({
          data:
            seenAtTs === null
              ? { events: [event('$latest', T0)], next_cursor: null }
              : {
                  events: [event('$old', expectedStartTs + 1)],
                  next_cursor: null,
                },
        })
      }),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
    const services = testServices()
    const { findByRole, findByText } = render(
      routedRoomPageWithJumpButton(services),
    )
    await findByText('body of $latest')
    const jump = await findByRole('button', { name: 'Jump' })
    await waitFor(() => expect(jump.hasAttribute('disabled')).toBe(false))

    fireEvent.click(jump)
    const dialog = await findByRole('dialog', { name: 'Jump to date' })
    fireEvent.input(within(dialog).getByLabelText('Date'), {
      target: { value: '2026-05-20' },
    })
    fireEvent.click(within(dialog).getByRole('button', { name: 'Jump' }))

    await waitFor(() => expect(seenAtTs).toBe(String(expectedAtTs)))
    expect(await findByText('body of $old')).toBeTruthy()
  })

  it('does not advance the summary read marker after a jump to date', async () => {
    const expectedStartTs = new Date(2026, 4, 20).getTime()
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: ROOM,
              name: 'Ops',
              topic: null,
              avatar_url: null,
              canonical_alias: null,
              last_activity_ts: T0,
              last_event_id: '$latest',
            },
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: '!other:hs',
              name: 'Other',
              topic: null,
              avatar_url: null,
              canonical_alias: null,
              last_activity_ts: T0 + 1,
              last_event_id: '$other',
            },
          ],
        }),
      ),
      http.get(TIMELINE_PATH, ({ request }) => {
        const atTs = new URL(request.url).searchParams.get('at_ts')
        return HttpResponse.json({
          data: {
            events:
              atTs === null
                ? [event('$latest', T0)]
                : [event('$old', expectedStartTs + 1)],
            next_cursor: null,
          },
        })
      }),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
    const services = testServices()
    const timeline = services.timelines.acquire(ACCOUNT, ROOM)
    const { findByRole, findByText } = render(
      routedRoomPageWithJumpButton(services),
    )
    await findByText('body of $latest')
    await waitFor(() =>
      expect(services.deviceState.readMarker(ACCOUNT, ROOM)).toEqual({
        eventId: '$latest',
        originTs: T0,
      }),
    )

    fireEvent.click(await findByRole('button', { name: 'Jump' }))
    const dialog = await findByRole('dialog', { name: 'Jump to date' })
    fireEvent.input(within(dialog).getByLabelText('Date'), {
      target: { value: '2026-05-20' },
    })
    fireEvent.click(within(dialog).getByRole('button', { name: 'Jump' }))

    await findByText('body of $old')
    await waitFor(() => expect(timeline.atEnd.value).toBe(false))
    services.rooms.noteTimelineEvent(event('$new-after-jump', T0 + 2))
    await new Promise((resolve) => setTimeout(resolve, 50))

    expect(services.deviceState.readMarker(ACCOUNT, ROOM)).toEqual({
      eventId: '$latest',
      originTs: T0,
    })
  })

  it('keeps the jump dialog open on invalid input and closes it with Escape', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: ROOM,
              name: 'Ops',
              topic: null,
              avatar_url: null,
              canonical_alias: null,
              last_activity_ts: T0,
              last_event_id: '$msg',
            },
          ],
        }),
      ),
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: { events: [event('$msg', T0)], next_cursor: null },
        }),
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
    const services = testServices()
    const { findByRole, queryByRole, findByText } = render(
      routedRoomPageWithJumpButton(services),
    )
    await findByText('body of $msg')
    const jump = await findByRole('button', { name: 'Jump' })
    await waitFor(() => expect(jump.hasAttribute('disabled')).toBe(false))

    fireEvent.click(jump)
    const dialog = await findByRole('dialog', { name: 'Jump to date' })
    fireEvent.input(within(dialog).getByLabelText('Date'), {
      target: { value: 'not-a-date' },
    })
    fireEvent.click(within(dialog).getByRole('button', { name: 'Jump' }))

    expect(await findByRole('alert')).toBeTruthy()
    fireEvent.keyDown(document.body, { key: 'Escape' })
    await waitFor(() =>
      expect(queryByRole('dialog', { name: 'Jump to date' })).toBeNull(),
    )
  })

  it('/jump opens the jump dialog from the composer', async () => {
    const { findByText, findByLabelText, findByRole } = renderRoom([
      event('$msg', T0),
    ])
    await findByText('body of $msg')
    const composer = await findByLabelText('Message Ops')

    fireEvent.input(composer, { target: { value: '/jump' } })
    fireEvent.keyDown(composer, { key: 'Enter' })

    expect(await findByRole('dialog', { name: 'Jump to date' })).toBeTruthy()
  })

  it('/jump with a date jumps directly from the composer', async () => {
    let seenAtTs: string | null = null
    let scrolledEventId: string | null = null
    Element.prototype.scrollIntoView = function () {
      scrolledEventId =
        this.closest('.event-row')?.getAttribute('data-event-id') ?? null
    }
    const expectedStartTs = new Date(2026, 0, 1).getTime()
    const expectedAtTs = new Date(2026, 0, 2).getTime() - 1 // end of local 2026-01-01
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: ROOM,
              name: 'Ops',
              topic: null,
              avatar_url: null,
              canonical_alias: null,
              last_activity_ts: T0,
              last_event_id: '$latest',
            },
          ],
        }),
      ),
      http.get(TIMELINE_PATH, ({ request }) => {
        seenAtTs = new URL(request.url).searchParams.get('at_ts')
        return HttpResponse.json({
          data:
            seenAtTs === null
              ? { events: [event('$latest', T0)], next_cursor: null }
              : {
                  events: [
                    event('$late-jan1', expectedAtTs - 1),
                    event('$jan1', expectedStartTs + 1),
                    event('$before-jan1', expectedStartTs - 1),
                  ],
                  next_cursor: null,
                },
        })
      }),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
    const { findByText, findByLabelText, queryByRole } = render(
      routedRoomPage(testServices()),
    )
    await findByText('body of $latest')
    const composer = await findByLabelText('Message Ops')

    fireEvent.input(composer, { target: { value: '/jump 2026-01-01' } })
    fireEvent.keyDown(composer, { key: 'Enter' })

    await waitFor(() => expect(seenAtTs).toBe(String(expectedAtTs)))
    expect(await findByText('body of $jan1')).toBeTruthy()
    expect(scrolledEventId).toBe('$jan1')
    expect(queryByRole('dialog', { name: 'Jump to date' })).toBeNull()
  })

  it('/jump with no messages on that date anchors to the newest earlier visible event', async () => {
    let seenAtTs: string | null = null
    let scrolledEventId: string | null = null
    Element.prototype.scrollIntoView = function () {
      scrolledEventId =
        this.closest('.event-row')?.getAttribute('data-event-id') ?? null
    }
    const juneStart = new Date(2026, 5, 1).getTime()
    const juneEnd = new Date(2026, 5, 2).getTime() - 1
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: ROOM,
              name: 'Ops',
              topic: null,
              avatar_url: null,
              canonical_alias: null,
              last_activity_ts: T0,
              last_event_id: '$latest',
            },
          ],
        }),
      ),
      http.get(TIMELINE_PATH, ({ request }) => {
        const atTs = new URL(request.url).searchParams.get('at_ts')
        seenAtTs = atTs
        return HttpResponse.json({
          data:
            atTs === null
              ? { events: [event('$latest', T0)], next_cursor: null }
              : {
                  events: [
                    event('$may31', juneStart - 1),
                    event('$may12', juneStart - 20 * DAY),
                  ],
                  next_cursor: null,
                },
        })
      }),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
    const { findByText, findByLabelText } = render(
      routedRoomPage(testServices()),
    )
    await findByText('body of $latest')
    const composer = await findByLabelText('Message Ops')

    fireEvent.input(composer, { target: { value: '/jump 2026-06-01' } })
    fireEvent.keyDown(composer, { key: 'Enter' })

    await waitFor(() => expect(seenAtTs).toBe(String(juneEnd)))
    await waitFor(() => expect(scrolledEventId).toBe('$may31'))
    expect(await findByText('body of $may31')).toBeTruthy()
  })

  it('renders sanitized formatted bodies and reveals spoilers on click', async () => {
    const { findByText, container } = renderRoom([
      event('$fmt', T0, {
        content: {
          msgtype: 'm.text',
          body: 'fallback',
          format: 'org.matrix.custom.html',
          formatted_body:
            '<strong>bold</strong> <span data-mx-spoiler>secret</span><script>x()</script>',
        },
      }),
    ])

    expect(await findByText('bold')).toBeTruthy()
    expect(container.querySelector('script')).toBeNull()

    const spoiler = container.querySelector('.spoiler')!
    expect(spoiler.classList.contains('spoiler-revealed')).toBe(false)
    fireEvent.click(spoiler)
    expect(spoiler.classList.contains('spoiler-revealed')).toBe(true)
  })

  it('renders emote and notice message kinds distinctly', async () => {
    const { findByText, container } = renderRoom([
      event('$emote', T0, {
        content: { msgtype: 'm.emote', body: 'waves' },
        body: 'waves',
      }),
      event('$notice', T0 + 1, {
        content: { msgtype: 'm.notice', body: 'bot says' },
        body: 'bot says',
      }),
    ])

    await findByText(/waves/)
    expect(container.querySelector('.event-body em')).toBeTruthy()
    expect((await findByText('bot says')).closest('.muted')).toBeTruthy()
  })

  it('renders sender avatars through the media proxy', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/${ACCOUNT}/hs/alice`,
        () =>
          new HttpResponse('avatar-bytes', {
            headers: { 'content-type': 'image/png' },
          }),
      ),
    )
    const { findByText, container } = renderRoom(
      [event('$avatar', T0, { sender: '@alice:hs' })],
      {
        members: [
          {
            user_id: '@alice:hs',
            membership: 'join',
            display_name: 'Alice',
            avatar_url: 'mxc://hs/alice',
          },
        ],
      },
    )

    expect(await findByText('Alice')).toBeTruthy()
    await waitFor(() => {
      const avatar = container.querySelector<HTMLImageElement>(
        '.event-row .user-avatar img',
      )
      expect(avatar?.src).toMatch(/^blob:/)
    })
  })

  it('renders colored fallback initials for senders without avatars', async () => {
    const { findByText, container } = renderRoom(
      [event('$fallback', T0, { sender: '@alice:hs' })],
      {
        members: [
          {
            user_id: '@alice:hs',
            membership: 'join',
            display_name: 'Alice',
          },
        ],
      },
    )

    expect(await findByText('Alice')).toBeTruthy()
    const avatar = container.querySelector<HTMLElement>(
      '.event-row .user-avatar',
    )!
    expect(avatar.textContent).toBe('A')
    expect(avatar.className).toMatch(/\buser-avatar-color-\d\b/)
    expect(avatar.querySelector('img')).toBeNull()
  })

  it('refreshes sender avatars after a live membership update', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/${ACCOUNT}/hs/alice-live`,
        () =>
          new HttpResponse('avatar-bytes', {
            headers: { 'content-type': 'image/png' },
          }),
      ),
    )
    const { services, findByText, container } = renderRoom(
      [event('$avatar-live-target', T0, { sender: '@alice:hs' })],
      {
        members: [
          {
            user_id: '@alice:hs',
            membership: 'join',
            display_name: 'Alice',
          },
        ],
      },
    )

    expect(await findByText('Alice')).toBeTruthy()
    expect(container.querySelector('.event-row .user-avatar img')).toBeNull()

    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/members`,
        () =>
          HttpResponse.json({
            data: [
              {
                user_id: '@alice:hs',
                membership: 'join',
                display_name: 'Alice',
                avatar_url: 'mxc://hs/alice-live',
              },
            ],
          }),
      ),
    )
    services.live.start()
    services.sockets[0].emitOpen()
    services.sockets[0].emitMessage(
      JSON.stringify({
        type: 'timeline.event',
        account_id: ACCOUNT,
        payload: event('$avatar-live-member', T0 + 1, {
          type: 'm.room.member',
          sender: '@alice:hs',
          state_key: '@alice:hs',
          content: {
            membership: 'join',
            avatar_url: 'mxc://hs/alice-live',
          },
        }),
      }),
    )

    await waitFor(() => {
      const avatar = container.querySelector<HTMLImageElement>(
        '.event-row .user-avatar img',
      )
      expect(avatar?.src).toMatch(/^blob:/)
    })
  })

  it('shows and clears live typing indicators from ephemeral frames', async () => {
    const { services, findByText, queryByText } = renderRoom(
      [event('$1', T0)],
      {
        members: [
          member('@alice:hs', 'join', 'Alice'),
          member('@bob:hs', 'join', 'Bob'),
        ],
      },
    )
    services.live.start()
    services.sockets[0].emitOpen()

    services.sockets[0].emitMessage(
      JSON.stringify({
        type: 'ephemeral.passthrough',
        account_id: ACCOUNT,
        payload: {
          room_id: ROOM,
          event_type: 'm.typing',
          content: { user_ids: ['@me:hs', '@alice:hs', '@bob:hs'] },
        },
      }),
    )

    expect(await findByText('Alice and Bob are typing')).toBeTruthy()

    services.sockets[0].emitMessage(
      JSON.stringify({
        type: 'ephemeral.passthrough',
        account_id: ACCOUNT,
        payload: {
          room_id: ROOM,
          event_type: 'm.typing',
          content: { user_ids: [] },
        },
      }),
    )
    await waitFor(() =>
      expect(queryByText('Alice and Bob are typing')).toBeNull(),
    )
  })

  it('renders public read receipts on my own messages only', async () => {
    const { services, findByText, queryByText } = renderRoom(
      [
        event('$mine', T0, { sender: '@me:hs', body: 'my message' }),
        event('$theirs', T0 + 1, {
          sender: '@alice:hs',
          body: 'their message',
        }),
      ],
      {
        members: [
          member('@alice:hs', 'join', 'Alice'),
          member('@bob:hs', 'join', 'Bob'),
        ],
      },
    )
    services.live.start()
    services.sockets[0].emitOpen()

    services.sockets[0].emitMessage(
      JSON.stringify({
        type: 'ephemeral.passthrough',
        account_id: ACCOUNT,
        payload: {
          room_id: ROOM,
          event_type: 'm.receipt',
          content: {
            $mine: {
              'm.read': {
                '@me:hs': { ts: 99 },
                '@alice:hs': { ts: 100 },
                '@bob:hs': { ts: 90 },
              },
            },
            $theirs: {
              'm.read': {
                '@bob:hs': { ts: 110 },
              },
            },
          },
        },
      }),
    )

    expect(await findByText('Seen by Alice')).toBeTruthy()
    expect(queryByText('Seen by Bob')).toBeNull()
  })

  it('excludes my own receipt from the seen-by summary count', async () => {
    const { services, findByText, queryByText } = renderRoom(
      [event('$mine', T0, { sender: '@me:hs', body: 'my message' })],
      {
        members: [
          member('@steve:hs', 'join', 'Steve'),
          member('@adam:hs', 'join', 'Adam'),
        ],
      },
    )
    services.live.start()
    services.sockets[0].emitOpen()

    services.sockets[0].emitMessage(
      JSON.stringify({
        type: 'ephemeral.passthrough',
        account_id: ACCOUNT,
        payload: {
          room_id: ROOM,
          event_type: 'm.receipt',
          content: {
            $mine: {
              'm.read': {
                '@me:hs': { ts: 120 },
                '@steve:hs': { ts: 110 },
                '@adam:hs': { ts: 100 },
              },
            },
          },
        },
      }),
    )

    expect(await findByText('Seen by Steve and Adam')).toBeTruthy()
    expect(queryByText('Seen by Steve, Adam, and 1 more')).toBeNull()
  })

  it('shows reaction tallies with my reactions highlighted', async () => {
    const { findByText, container } = renderRoom([
      event('$r', T0, {
        reactions: {
          '👍': { count: 2, me: true, senders: ['@a:hs', '@me:hs'] },
          '🎉': { count: 1, me: false, senders: ['@b:hs'] },
        },
      }),
    ])

    expect(await findByText('👍 2')).toBeTruthy()
    expect(await findByText('🎉 1')).toBeTruthy()
    expect(
      container.querySelector('.reaction-chip.mine')!.textContent,
    ).toContain('👍')
  })

  it('shows bounded reaction sender popover with display names', async () => {
    const senders = Array.from(
      { length: 12 },
      (_, index) => `@u${index + 1}:hs`,
    )
    const members = senders.map((user_id, index) => ({
      user_id,
      membership: 'join',
      display_name: `User ${index + 1}`,
    }))
    const { findByRole, findByText } = renderRoom(
      [
        event('$r', T0, {
          reactions: {
            '🔥': { count: 12, me: false, senders },
          },
        }),
      ],
      { members },
    )

    const chip = await findByText('🔥 12')
    fireEvent.mouseEnter(chip.parentElement!)
    expect((await findByRole('tooltip')).textContent).toBe(
      '🔥: User 1, User 2, User 3, User 4, User 5, User 6, User 7, User 8, User 9, User 10, and 2 more',
    )
  })

  it('shows reaction sender popover on touch hold without toggling the reaction', async () => {
    vi.useFakeTimers()
    let reactionPosts = 0
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        () => {
          reactionPosts += 1
          return HttpResponse.json({ data: { event_id: '$rx' } })
        },
      ),
    )
    const { findByText, findByRole } = renderRoom([
      event('$r', T0, {
        reactions: {
          '🔥': { count: 1, me: false, senders: ['@alice:hs'] },
        },
      }),
    ])

    const chip = await findByText('🔥 1')
    fireEvent.touchStart(chip)
    vi.advanceTimersByTime(450)
    expect((await findByRole('tooltip')).textContent).toBe('🔥: @alice:hs')

    fireEvent.touchEnd(chip)
    fireEvent.click(chip)
    expect(reactionPosts).toBe(0)
  })

  it('reacts on the tap after a long press that never produced a click', async () => {
    vi.useFakeTimers()
    let reactionPosts = 0
    const reacted = event('$r', T0, {
      reactions: { '🔥': { count: 2, me: true, senders: ['@alice:hs'] } },
    })
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        () => {
          reactionPosts += 1
          return HttpResponse.json({ data: { event_id: '$rx' } })
        },
      ),
      // The toggle reconciles by re-reading its target.
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: reacted }),
      ),
    )
    const { findByText } = renderRoom([
      event('$r', T0, {
        reactions: {
          '🔥': { count: 1, me: false, senders: ['@alice:hs'] },
        },
      }),
    ])

    // Hold to read the senders, then lift without tapping the chip.
    const chip = await findByText('🔥 1')
    fireEvent.touchStart(chip)
    vi.advanceTimersByTime(450)
    fireEvent.touchEnd(chip)
    vi.useRealTimers()

    // The next tap is a real one and must toggle.
    fireEvent.touchStart(chip)
    fireEvent.touchEnd(chip)
    fireEvent.click(chip)

    await waitFor(() => expect(reactionPosts).toBe(1))
  })

  it('keeps a live reaction on the newest message visible when pinned to bottom', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({
          data: event('$latest', T0, {
            reactions: {
              '🔥': { count: 1, me: false, senders: ['@bob:hs'] },
            },
          }),
        }),
      ),
    )
    const { services, findByText, container } = renderRoom([
      event('$latest', T0),
    ])
    await findByText('body of $latest')
    const timeline = container.querySelector<HTMLElement>('.timeline')!
    setTimelineScrollGeometry(timeline, {
      clientHeight: 50,
      scrollHeight: () =>
        container.querySelector('.reaction-chip') === null ? 100 : 120,
    })
    timeline.scrollTop = 50

    services.live.start()
    services.sockets[0].emitOpen()
    services.sockets[0].emitMessage(
      JSON.stringify({
        type: 'timeline.event',
        account_id: ACCOUNT,
        payload: event('$rx', T0 + 1, {
          sender: '@bob:hs',
          type: 'm.reaction',
          body: null,
          relates_to: {
            rel_type: 'm.annotation',
            event_id: '$latest',
            key: '🔥',
          },
        }),
      }),
    )

    expect(await findByText('🔥 1')).toBeTruthy()
    await waitFor(() => expect(timeline.scrollTop).toBe(120))
  })

  it('keeps the newest message visible when room content grows after mount', async () => {
    installResizeObserver()
    let scrollHeight = 100
    const { findByText, container } = renderRoom([event('$latest', T0)])
    await findByText('body of $latest')
    const timeline = container.querySelector<HTMLElement>('.timeline')!
    setTimelineScrollGeometry(timeline, {
      clientHeight: 50,
      scrollHeight: () => scrollHeight,
    })
    timeline.scrollTop = 50

    scrollHeight = 130
    await triggerObservedResizeFrame()

    expect(timeline.scrollTop).toBe(130)
  })

  it('keeps the newest message visible when the soft keyboard shrinks the scroller', async () => {
    // The keyboard shrinks the scroller's *own* box (its flex parent loses
    // height to the shrunk visual viewport) — the content element inside it
    // never resizes. A fake that fired every observer regardless of its
    // target (like `installResizeObserver` above) couldn't tell this case
    // apart from ordinary content growth, so this one dispatches only to
    // whichever observer is watching a given target, the way a real
    // `ResizeObserver` would.
    const byTarget = new Map<Element, ResizeObserverCallback>()
    class TargetedResizeObserver {
      #callback: ResizeObserverCallback
      constructor(callback: ResizeObserverCallback) {
        this.#callback = callback
      }
      observe(target: Element) {
        byTarget.set(target, this.#callback)
      }
      unobserve(target: Element) {
        byTarget.delete(target)
      }
      disconnect() {}
    }
    globalThis.ResizeObserver =
      TargetedResizeObserver as unknown as typeof ResizeObserver
    // Fire the scheduled pin frame synchronously: the assertion needs no real
    // await this way, so nothing else gets a turn to run in between and
    // mask (or fake) the effect under test.
    const originalRaf = globalThis.requestAnimationFrame
    globalThis.requestAnimationFrame = ((callback: FrameRequestCallback) => {
      callback(0)
      return 0
    }) as typeof requestAnimationFrame

    try {
      const { findByText, container } = renderRoom([event('$latest', T0)])
      await findByText('body of $latest')
      const timeline = container.querySelector<HTMLElement>('.timeline')!
      const eventList = container.querySelector<HTMLElement>('.event-list')!
      setTimelineScrollGeometry(timeline, {
        clientHeight: 50,
        scrollHeight: () => 100,
      })
      timeline.scrollTop = 50

      // Only the scroller's own box shrinks; the content observer (on
      // `.event-list`) is left untouched, so this isolates the fix under
      // test.
      setTimelineScrollGeometry(timeline, {
        clientHeight: 30,
        scrollHeight: () => 100,
      })
      expect(byTarget.has(eventList)).toBe(true)
      byTarget.get(timeline)?.([], null as unknown as ResizeObserver)

      expect(timeline.scrollTop).toBe(100)
    } finally {
      globalThis.requestAnimationFrame = originalRaf
    }
  })

  it('re-pins to bottom even when a scroll event mid-keyboard-animation clobbers stickToBottom (issue found live on iOS)', async () => {
    // The observed failure on a real phone: focusing the composer forces an
    // immediate scroll to the (pre-keyboard) bottom, and that scroll's own
    // `scroll` event can land *after* the keyboard has already started
    // shrinking the scroller but *before* its resize has settled. `onScroll`
    // then computes "not at bottom" from a transient `clientHeight` and
    // clobbers `stickToBottom` to false — so when the keyboard's resize does
    // settle, the plain `stickToBottom` guard alone would wrongly skip the
    // pin. `keyboardPin`, captured at focus time before any of this noise,
    // is what recovers it.
    //
    // The trace that caught this live also showed a second failure mode: an
    // unrelated, harmless scroller-resize fires *before* the keyboard's real
    // shrink (a minor content reflow around focus time), and a version of
    // this fix that cleared `keyboardPin` on the first resize it saw burned
    // the override on that harmless one — leaving nothing to rescue the real
    // resize a moment later. This test includes that intermediate resize to
    // cover it.
    const byTarget = new Map<Element, ResizeObserverCallback>()
    class TargetedResizeObserver {
      #callback: ResizeObserverCallback
      constructor(callback: ResizeObserverCallback) {
        this.#callback = callback
      }
      observe(target: Element) {
        byTarget.set(target, this.#callback)
      }
      unobserve(target: Element) {
        byTarget.delete(target)
      }
      disconnect() {}
    }
    globalThis.ResizeObserver =
      TargetedResizeObserver as unknown as typeof ResizeObserver
    // A queueing (not auto-running) fake: `pinLiveTimelineAfterComposerFocus`
    // schedules its own double-rAF on focus, and it must not fire before this
    // test is ready for it — `flushRaf` runs whatever is pending, on demand.
    const originalRaf = globalThis.requestAnimationFrame
    let rafQueue: FrameRequestCallback[] = []
    globalThis.requestAnimationFrame = ((callback: FrameRequestCallback) => {
      rafQueue.push(callback)
      return rafQueue.length
    }) as typeof requestAnimationFrame
    const flushRaf = () => {
      const queued = rafQueue
      rafQueue = []
      queued.forEach((callback) => callback(0))
    }

    try {
      const { findByText, container } = renderRoom([event('$latest', T0)])
      await findByText('body of $latest')
      const timeline = container.querySelector<HTMLElement>('.timeline')!
      const textarea = container.querySelector('textarea')!
      let clientHeight = 750
      Object.defineProperty(timeline, 'clientHeight', {
        configurable: true,
        get: () => clientHeight,
      })
      Object.defineProperty(timeline, 'scrollHeight', {
        configurable: true,
        value: 7110,
      })
      timeline.scrollTop = 6360 // exactly at bottom: 7110 - 6360 - 750 = 0

      // Tapping the composer — captures `keyboardPin` while genuinely at
      // the live end. (Its own focus-triggered pin is queued, not run yet.)
      textarea.focus()

      // A harmless, unrelated resize fires first — content reflowing near
      // focus time, nothing to do with the keyboard. `keyboardPin` must
      // survive this; it is not yet the resize the fix is watching for.
      clientHeight = 746
      byTarget.get(timeline)?.([], null as unknown as ResizeObserver)

      // The keyboard starts shrinking the box; a scroll event lands
      // mid-animation, at an intermediate clientHeight, before the resize
      // settles. This is what flips `stickToBottom` false on the real
      // device.
      clientHeight = 460
      fireEvent.scroll(timeline)
      expect(timeline.scrollTop).toBe(6360) // unmoved by the stray scroll

      // The keyboard's resize settles at its final height.
      clientHeight = 331
      byTarget.get(timeline)?.([], null as unknown as ResizeObserver)
      flushRaf()

      expect(timeline.scrollTop).toBe(7110)
    } finally {
      globalThis.requestAnimationFrame = originalRaf
    }
  })

  it('does not arm keyboardPin from an input outside the composer', async () => {
    // The focus listener sits on `document` and `RoomPage` does not remount on
    // in-room navigation, so before scoping, any text control in the page armed
    // the pin. `JumpDialog`'s date inputs are the damaging case: a resize
    // inside the pin window would force the reader back to the live end —
    // exactly what jumping to a date is meant to get away from.
    const byTarget = new Map<Element, ResizeObserverCallback>()
    class TargetedResizeObserver {
      #callback: ResizeObserverCallback
      constructor(callback: ResizeObserverCallback) {
        this.#callback = callback
      }
      observe(target: Element) {
        byTarget.set(target, this.#callback)
      }
      unobserve(target: Element) {
        byTarget.delete(target)
      }
      disconnect() {}
    }
    globalThis.ResizeObserver =
      TargetedResizeObserver as unknown as typeof ResizeObserver
    const originalRaf = globalThis.requestAnimationFrame
    let rafQueue: FrameRequestCallback[] = []
    globalThis.requestAnimationFrame = ((callback: FrameRequestCallback) => {
      rafQueue.push(callback)
      return rafQueue.length
    }) as typeof requestAnimationFrame
    const flushRaf = () => {
      const queued = rafQueue
      rafQueue = []
      queued.forEach((callback) => callback(0))
    }

    try {
      const { findByText, container } = renderRoom([event('$latest', T0)])
      await findByText('body of $latest')
      const timeline = container.querySelector<HTMLElement>('.timeline')!
      let clientHeight = 750
      Object.defineProperty(timeline, 'clientHeight', {
        configurable: true,
        get: () => clientHeight,
      })
      Object.defineProperty(timeline, 'scrollHeight', {
        configurable: true,
        value: 7110,
      })

      // The reader is at the live end when they reach for "jump to date".
      timeline.scrollTop = 6360 // 7110 - 6360 - 750 = 0
      fireEvent.scroll(timeline)

      // Focusing a text control that is in the page but not in the composer.
      // This is the moment that used to capture `stickToBottom` — true, right
      // now — and hold it for the whole pin window.
      const stream = container.querySelector<HTMLElement>('.room-stream')!
      const outsider = document.createElement('input')
      stream.appendChild(outsider)
      outsider.focus()

      // They jump back into history, which is the entire point of the feature.
      timeline.scrollTop = 1000
      fireEvent.scroll(timeline)

      // A resize lands inside what would have been the pin window.
      clientHeight = 331
      byTarget.get(timeline)?.([], null as unknown as ResizeObserver)
      flushRaf()

      // No pin was armed, so the reader stays exactly where they scrolled,
      // not just "somewhere other than the live end".
      expect(timeline.scrollTop).toBe(1000)
    } finally {
      globalThis.requestAnimationFrame = originalRaf
    }
  })

  it('lets keyboardPin expire, so a reader who scrolled away stays there', async () => {
    // `keyboardPin` is a bounded window (`KEYBOARD_PIN_MS`), not a flag that
    // rides along with focus forever — once it expires, a resize is judged
    // purely on the reader's actual position again.
    const byTarget = new Map<Element, ResizeObserverCallback>()
    class TargetedResizeObserver {
      #callback: ResizeObserverCallback
      constructor(callback: ResizeObserverCallback) {
        this.#callback = callback
      }
      observe(target: Element) {
        byTarget.set(target, this.#callback)
      }
      unobserve(target: Element) {
        byTarget.delete(target)
      }
      disconnect() {}
    }
    globalThis.ResizeObserver =
      TargetedResizeObserver as unknown as typeof ResizeObserver
    // The true native rAF, captured *before* faking timers — `vi.useFakeTimers`
    // fakes `requestAnimationFrame` too, and capturing after it (as an earlier
    // version of this test did) restores a disconnected fake in `finally`,
    // wedging every later test in this file that waits on a real animation
    // frame.
    const originalRaf = globalThis.requestAnimationFrame
    vi.useFakeTimers()
    // Queued, not run automatically or discarded: `pinLiveTimelineAfterComposer
    // Focus`'s own focus-triggered double-rAF (unconditional — it forces the
    // scroller to its bottom regardless of `stickToBottom`) must not fire
    // while staging the scenario, but `scheduleResizePin`'s rAF still needs to
    // be flushable afterward, or "no pin happened" would hold vacuously
    // whether or not the fix actually ran.
    let rafQueue: FrameRequestCallback[] = []
    globalThis.requestAnimationFrame = ((callback: FrameRequestCallback) => {
      rafQueue.push(callback)
      return rafQueue.length
    }) as typeof requestAnimationFrame
    const flushRaf = () => {
      const queued = rafQueue
      rafQueue = []
      queued.forEach((callback) => callback(0))
    }

    try {
      const { findByText, container } = renderRoom([event('$latest', T0)])
      await findByText('body of $latest')
      const timeline = container.querySelector<HTMLElement>('.timeline')!
      const textarea = container.querySelector('textarea')!
      let clientHeight = 750
      Object.defineProperty(timeline, 'clientHeight', {
        configurable: true,
        get: () => clientHeight,
      })
      Object.defineProperty(timeline, 'scrollHeight', {
        configurable: true,
        value: 7110,
      })
      timeline.scrollTop = 6360 // at the bottom

      textarea.focus() // captures keyboardPin=true
      rafQueue = [] // discard the focus pin's own queued double-rAF

      // The reader scrolls away for real, well within the keyboardPin window.
      timeline.scrollTop = 1000
      fireEvent.scroll(timeline)

      // The window closes with no further keyboard resize.
      vi.advanceTimersByTime(1000)

      // A later, unrelated resize must not snap them back to the bottom —
      // keyboardPin has expired, so only their actual position counts.
      clientHeight = 500
      byTarget.get(timeline)?.([], null as unknown as ResizeObserver)
      flushRaf()

      expect(timeline.scrollTop).toBe(1000)
    } finally {
      vi.useRealTimers()
      globalThis.requestAnimationFrame = originalRaf
    }
  })

  it('does not repin resized room content after the user scrolls back', async () => {
    installResizeObserver()
    let scrollHeight = 100
    const { findByText, container } = renderRoom([event('$latest', T0)])
    await findByText('body of $latest')
    const timeline = container.querySelector<HTMLElement>('.timeline')!
    setTimelineScrollGeometry(timeline, {
      clientHeight: 50,
      scrollHeight: () => scrollHeight,
    })
    timeline.scrollTop = 0
    fireEvent.scroll(timeline)

    scrollHeight = 130
    await triggerObservedResizeFrame()

    expect(timeline.scrollTop).toBe(0)
  })

  it('does not write scrollTop again when resized content is already pinned', async () => {
    installResizeObserver()
    const scrollHeight = 100
    const { findByText, container } = renderRoom([event('$latest', T0)])
    await findByText('body of $latest')
    const timeline = container.querySelector<HTMLElement>('.timeline')!
    setTimelineScrollGeometry(timeline, {
      clientHeight: 50,
      scrollHeight: () => scrollHeight,
    })
    let writes = 0
    let top = 50
    Object.defineProperty(timeline, 'scrollTop', {
      configurable: true,
      get: () => top,
      set: (value) => {
        writes += 1
        top = value
      },
    })

    await triggerObservedResizeFrame()

    expect(writes).toBe(0)
    expect(timeline.scrollTop).toBe(50)
  })

  it('keeps my new reaction on the newest message visible when pinned to bottom', async () => {
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/events/:eventId/reactions`,
        () => HttpResponse.json({ data: { event_id: '$rx' } }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({
          data: event('$latest', T0, {
            reactions: {
              '👍': { count: 1, me: true, senders: ['@me:hs'] },
            },
          }),
        }),
      ),
    )
    const { findByRole, findByText, container } = renderRoom([
      event('$latest', T0),
    ])
    await findByText('body of $latest')
    const timeline = container.querySelector<HTMLElement>('.timeline')!
    setTimelineScrollGeometry(timeline, {
      clientHeight: 50,
      scrollHeight: () =>
        container.querySelector('.reaction-chip') === null ? 100 : 120,
    })
    timeline.scrollTop = 50

    fireEvent.click(await findByRole('button', { name: 'React' }))
    fireEvent.click(await findByRole('button', { name: '👍' }))

    expect(await findByText('👍 1')).toBeTruthy()
    await waitFor(() => expect(timeline.scrollTop).toBe(120))
  })

  it('renders reply context from the loaded slice', async () => {
    const { findByText, container } = renderRoom([
      event('$target', T0, { body: 'original message' }),
      event('$reply', T0 + 1, {
        relates_to: { 'm.in_reply_to': { event_id: '$target' } },
      }),
    ])

    await findByText('body of $reply')
    const quote = container.querySelector('.reply-context')!
    expect(quote.textContent).toContain('original message')
    const link = quote.querySelector<HTMLAnchorElement>('.reply-context-link')
    expect(link?.getAttribute('href')).toBe(
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24target`,
    )
    expect(link?.getAttribute('aria-label')).toBe(
      'Jump to original message from @alice:hs',
    )
  })

  it('renders formatted markdown in reply context anchors', async () => {
    const { findByText, container } = renderRoom([
      event('$target', T0, {
        body: '**bold** anchor',
        content: {
          msgtype: 'm.text',
          body: '**bold** anchor',
          format: 'org.matrix.custom.html',
          formatted_body: '<p><strong>bold</strong> anchor</p>',
        },
      }),
      event('$reply', T0 + 1, {
        relates_to: { 'm.in_reply_to': { event_id: '$target' } },
      }),
    ])

    await findByText('body of $reply')
    const quote = container.querySelector('.reply-context')!
    expect(quote.querySelector('strong')?.textContent).toBe('bold')
    expect(quote.textContent).not.toContain('**bold**')
  })

  it('applies the modern link style inside reply context anchors', async () => {
    const { findByText, container } = renderRoom([
      event('$target', T0, {
        body: 'see https://example.org/docs',
        content: {
          msgtype: 'm.text',
          body: 'see https://example.org/docs',
          format: 'org.matrix.custom.html',
          formatted_body:
            '<p>see <a href="https://example.org/docs">https://example.org/docs</a></p>',
        },
      }),
      event('$reply', T0 + 1, {
        relates_to: { 'm.in_reply_to': { event_id: '$target' } },
      }),
    ])

    await findByText('body of $reply')
    const anchor = container.querySelector<HTMLAnchorElement>(
      '.reply-context a[href="https://example.org/docs"]',
    )
    expect(anchor).not.toBeNull()
    expect(anchor?.closest('.event-body')).toBeNull()
    expect(anchor?.closest('.reply-context')).toBeTruthy()
  })

  it('opens edit history from the (edited) marker', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId/edits`,
        () =>
          HttpResponse.json({
            data: [
              event('$e1', T0 + 10, {
                content: {
                  'm.new_content': { msgtype: 'm.text', body: 'first version' },
                },
                body: '* first version',
              }),
            ],
          }),
      ),
    )
    const { findByText, findByRole, getByRole, container } = renderRoom([
      event('$edited', T0, { edited: true, edit_count: 1 }),
    ])

    fireEvent.click(await findByText('(edited)'))
    const dialog = await findByRole('dialog')
    expect(dialog.closest('.event-row')).toBeNull()
    expect(container.querySelector('.overlay')).toBeNull()
    expect(await findByText('first version')).toBeTruthy()

    fireEvent.click(getByRole('button', { name: 'Close' }))
  })

  describe('scroll anchoring', () => {
    /** jsdom does no layout, so both the geometry and the observer are faked. */
    function fakeRect(el: Element, top: number, height: number) {
      el.getBoundingClientRect = () =>
        ({
          top,
          bottom: top + height,
          height,
          left: 0,
          right: 0,
          width: 0,
          x: 0,
          y: top,
          toJSON: () => ({}),
        }) as DOMRect
    }

    function installResizeObserver() {
      const callbacks: ResizeObserverCallback[] = []
      class FakeResizeObserver {
        constructor(callback: ResizeObserverCallback) {
          callbacks.push(callback)
        }
        observe() {}
        disconnect() {}
        unobserve() {}
      }
      vi.stubGlobal('ResizeObserver', FakeResizeObserver)
      // Both the bottom pin and the anchor observe the list; firing all of
      // them is what the browser would do.
      return () => {
        for (const callback of callbacks) {
          callback([], null as unknown as ResizeObserver)
        }
      }
    }

    /** A scroller parked in history, with three rows laid out below its top. */
    async function scrolledBackTimeline() {
      const fire = installResizeObserver()
      const view = renderRoom([
        event('$1', T0),
        event('$2', T0 + 1),
        event('$3', T0 + 2),
      ])
      await view.findByText('body of $3')
      const el = view.container.querySelector('.timeline') as HTMLElement
      let scrollTop = 800
      Object.defineProperty(el, 'scrollHeight', {
        configurable: true,
        value: 2000,
      })
      Object.defineProperty(el, 'clientHeight', {
        configurable: true,
        value: 500,
      })
      Object.defineProperty(el, 'scrollTop', {
        configurable: true,
        get: () => scrollTop,
        set: (next: number) => {
          scrollTop = next
        },
      })
      fakeRect(el, 0, 500)
      const rows = [
        ...view.container.querySelectorAll<HTMLElement>('li.event-row'),
      ]
      rows.forEach((row, index) => fakeRect(row, 10 + index * 50, 50))
      return { ...view, el, rows, fire, top: () => scrollTop }
    }

    afterEach(() => vi.unstubAllGlobals())

    it('puts back a shift caused by a row above the reader growing', async () => {
      const { el, rows, fire, top } = await scrolledBackTimeline()

      // Scrolling away from the bottom takes the topmost visible row as the
      // anchor — `$1`, sitting 10px below the scroller's top edge.
      fireEvent.scroll(el)
      expect(top()).toBe(800)

      // A row above it corrects from its placeholder height to its real one,
      // pushing everything down 85px — the shift measured on the phone.
      rows.forEach((row, index) => fakeRect(row, 95 + index * 50, 50))
      fire()

      // Scrolled down by exactly as much as the content moved down, so what
      // the reader was looking at has not moved at all.
      expect(top()).toBe(885)
    })

    it('leaves the bottom pin alone when the reader is at the live end', async () => {
      const { rows, fire, top } = await scrolledBackTimeline()
      // No scroll event, so `stickToBottom` is still the mount default: the
      // reader is at the tail and the bottom pin owns the scroll position.
      rows.forEach((row, index) => fakeRect(row, 95 + index * 50, 50))
      fire()

      expect(top()).toBe(800)
    })

    it('measures growth net of the reader scrolling in between', async () => {
      // The anchor is taken once and outlives the scrolling that follows —
      // it has to, since retaking it mid-scroll forces the layout that
      // renders the incoming rows, and so records their growth as already
      // having happened. The reader's own movement must cancel out exactly.
      const { el, rows, fire, top } = await scrolledBackTimeline()
      fireEvent.scroll(el)

      // Scrolled back 200px: every row moves down by 200, nothing grew.
      el.scrollTop = 600
      rows.forEach((row, index) => fakeRect(row, 210 + index * 50, 50))
      fire()
      expect(top()).toBe(600)

      // Now a row above grows 85px, with no further scrolling. Only the
      // growth is put back — not the 200px the reader travelled.
      rows.forEach((row, index) => fakeRect(row, 295 + index * 50, 50))
      fire()
      expect(top()).toBe(685)
    })

    it('anchors below a row straddling the top edge, not to it', async () => {
      // The straddling row is the one being revealed and rendered for the
      // first time, so it is the likeliest to correct its own height. Its top
      // does not move when it grows downward, so anchoring to it would measure
      // no shift while everything below it moved.
      const { el, rows, fire, top } = await scrolledBackTimeline()
      fakeRect(rows[0], -20, 50) // straddles: top above the edge, bottom below
      fakeRect(rows[1], 30, 50)
      fakeRect(rows[2], 80, 50)
      fireEvent.scroll(el)

      // The straddling row grows by 85px; rows below it move down.
      fakeRect(rows[0], -20, 135)
      fakeRect(rows[1], 115, 50)
      fakeRect(rows[2], 165, 50)
      fire()

      expect(top()).toBe(885)
    })

    it('takes a fresh anchor when the held row leaves the slice', async () => {
      const { el, rows, fire, top } = await scrolledBackTimeline()
      fireEvent.scroll(el)

      // The anchor row is removed — a slice replacement, or the retained-slice
      // trim. Nothing to hold, so the scroll position is left where it is.
      rows[0].remove()
      rows.slice(1).forEach((row, index) => fakeRect(row, 95 + index * 50, 50))
      fire()

      expect(top()).toBe(800)
    })
  })

  describe('automatic scroll-back', () => {
    /**
     * jsdom has no `IntersectionObserver`, which is what normally sends the
     * timeline down the button fallback. These tests install one so the
     * observer path itself can be exercised; `fire` plays the "top sentinel
     * came into view" callback. Element rects are all zero in jsdom, so the
     * chain's own re-measure always reads "still on screen" — which is what
     * makes the cap the thing under test.
     */
    function installIntersectionObserver() {
      const callbacks: IntersectionObserverCallback[] = []
      class FakeIntersectionObserver {
        constructor(callback: IntersectionObserverCallback) {
          callbacks.push(callback)
        }
        observe() {}
        disconnect() {}
        unobserve() {}
        takeRecords() {
          return []
        }
      }
      vi.stubGlobal('IntersectionObserver', FakeIntersectionObserver)
      return () => {
        // The top sentinel is the first observer RoomPage creates.
        callbacks[0]?.(
          [{ isIntersecting: true } as IntersectionObserverEntry],
          null as unknown as IntersectionObserver,
        )
      }
    }

    afterEach(() => vi.unstubAllGlobals())

    /** A state event: fetched and stored, but filtered out of the view. */
    const hidden = (id: string, ts: number) =>
      event(id, ts, { type: 'm.room.member', state_key: '@bob:hs' })

    it('keeps paging across pages that add no visible rows', async () => {
      // Every page is state events, so the timeline's height never changes
      // and no second intersection callback would ever arrive. Before the
      // chain, scroll-back stalled here after exactly one page.
      let calls = 0
      server.use(
        http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
          HttpResponse.json({ data: [] }),
        ),
        http.get(TIMELINE_PATH, () => {
          calls += 1
          return HttpResponse.json({
            data: {
              events: [hidden(`$s${calls}`, T0 - calls)],
              next_cursor: `older-${calls}`,
            },
          })
        }),
      )
      window.history.replaceState(
        null,
        '',
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
      )
      const fire = installIntersectionObserver()
      render(routedRoomPage(testServices()))
      await waitFor(() => expect(calls).toBe(1))

      fire()

      // One arrival at the top pulls up to the cap, then leaves the rest to
      // the button rather than walking history unattended.
      await waitFor(() => expect(calls).toBe(6))
      await new Promise((resolve) => setTimeout(resolve, 20))
      expect(calls).toBe(6)
    })

    it('keeps paging after the slice reaches its retained cap', async () => {
      // Full pages, so the store starts trimming its newest end partway
      // through. A chain that measured progress by slice *length* would read
      // the trim as "nothing loaded" and stall from there on.
      let calls = 0
      server.use(
        http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
          HttpResponse.json({ data: [] }),
        ),
        http.get(TIMELINE_PATH, () => {
          calls += 1
          return HttpResponse.json({
            data: {
              events: Array.from({ length: 50 }, (_, i) =>
                hidden(`$s${calls}-${i}`, T0 - calls * 100 - i),
              ),
              next_cursor: `older-${calls}`,
            },
          })
        }),
      )
      window.history.replaceState(
        null,
        '',
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
      )
      const fire = installIntersectionObserver()
      render(routedRoomPage(testServices()))
      await waitFor(() => expect(calls).toBe(1))

      // Four arrivals at the top: 20 pages, well past the 12-page cap.
      for (const expected of [6, 11, 16, 21]) {
        fire()
        await waitFor(() => expect(calls).toBe(expected))
      }
    })

    it('crosses a page that lands but adds nothing to the slice', async () => {
      // The reported stall: scroll-back stopping at the same message on every
      // browser. A page can advance the cursor while contributing no events
      // at all; reading that as "no progress" ended the chain on the message
      // before it, and only the button got past it.
      let calls = 0
      server.use(
        http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
          HttpResponse.json({ data: [] }),
        ),
        http.get(TIMELINE_PATH, () => {
          calls += 1
          return HttpResponse.json({
            data: {
              // The third page is empty but the cursor moves on.
              events:
                calls === 3 ? [] : [event(`$m${calls}`, T0 - calls * 100)],
              next_cursor: `older-${calls}`,
            },
          })
        }),
      )
      window.history.replaceState(
        null,
        '',
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
      )
      const fire = installIntersectionObserver()
      const { findByText } = render(routedRoomPage(testServices()))
      await waitFor(() => expect(calls).toBe(1))

      fire()

      await waitFor(() => expect(calls).toBe(6))
      // The pages past the empty one are loaded, not stranded behind it.
      expect(await findByText('body of $m5')).toBeTruthy()
    })

    it('a scroll resumes the chain without a fresh intersection', async () => {
      // The chain must not depend on `IntersectionObserver` reporting a
      // *change*: once it has stopped with the sentinel still on screen, no
      // such change is coming, and only the reader's scroll can say "more".
      let calls = 0
      server.use(
        http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
          HttpResponse.json({ data: [] }),
        ),
        http.get(TIMELINE_PATH, () => {
          calls += 1
          return HttpResponse.json({
            data: {
              events: [event(`$m${calls}`, T0 - calls * 100)],
              next_cursor: `older-${calls}`,
            },
          })
        }),
      )
      window.history.replaceState(
        null,
        '',
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
      )
      installIntersectionObserver()
      const { container } = render(routedRoomPage(testServices()))
      await waitFor(() => expect(calls).toBe(1))

      const scroller = container.querySelector('.timeline')!
      fireEvent.scroll(scroller)

      // One chain's worth, from a scroll alone — no observer callback fired.
      await waitFor(() => expect(calls).toBe(6))

      // And the next scroll starts another, so hitting the cap is never a
      // dead end for a reader who keeps scrolling.
      fireEvent.scroll(scroller)
      await waitFor(() => expect(calls).toBe(11))
    })

    it('stops the chain at the beginning of history', async () => {
      let calls = 0
      server.use(
        http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
          HttpResponse.json({ data: [] }),
        ),
        http.get(TIMELINE_PATH, () => {
          calls += 1
          return HttpResponse.json({
            data: {
              events: [hidden(`$s${calls}`, T0 - calls)],
              next_cursor: calls >= 3 ? null : `older-${calls}`,
            },
          })
        }),
      )
      window.history.replaceState(
        null,
        '',
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
      )
      const fire = installIntersectionObserver()
      const { findByText } = render(routedRoomPage(testServices()))
      await waitFor(() => expect(calls).toBe(1))

      fire()

      expect(await findByText('Beginning of room history.')).toBeTruthy()
      await new Promise((resolve) => setTimeout(resolve, 20))
      expect(calls).toBe(3)
    })
  })

  it('load-older prepends the older page', async () => {
    let calls = 0
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(TIMELINE_PATH, ({ request }) => {
        calls += 1
        const cursor = new URL(request.url).searchParams.get('cursor')
        if (cursor === null) {
          return HttpResponse.json({
            data: { events: [event('$2', T0 + 1)], next_cursor: 'older' },
          })
        }
        return HttpResponse.json({
          data: { events: [event('$1', T0)], next_cursor: null },
        })
      }),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
    const services = testServices()
    const { findByRole, findByText, container } = render(
      routedRoomPage(services),
    )

    fireEvent.click(await findByRole('button', { name: 'Load older messages' }))

    expect(await findByText('Beginning of room history.')).toBeTruthy()
    const bodies = [...container.querySelectorAll('.event-body')].map(
      (el) => el.textContent,
    )
    expect(bodies).toEqual(['body of $1', 'body of $2'])
    expect(calls).toBe(2)
  })

  it('per-row UI state stays with its message across a load-older prepend (WCR-01)', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(TIMELINE_PATH, ({ request }) => {
        const cursor = new URL(request.url).searchParams.get('cursor')
        if (cursor === null) {
          return HttpResponse.json({
            data: { events: [event('$2', T0 + 1)], next_cursor: 'older' },
          })
        }
        return HttpResponse.json({
          data: { events: [event('$1', T0)], next_cursor: null },
        })
      }),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
    const services = testServices()
    const { findByRole, findByText, container } = render(
      routedRoomPage(services),
    )

    // Open the reaction picker on $2, then prepend an older page. The rows
    // are keyed fragments; an index-matched reconcile would leave the open
    // picker attached to whatever message lands at that position ($1).
    fireEvent.click(await findByRole('button', { name: 'React' }))
    expect(
      container
        .querySelector('li.event-row[data-event-id="$2"]')!
        .querySelector('.reaction-picker'),
    ).not.toBeNull()

    fireEvent.click(await findByRole('button', { name: 'Load older messages' }))
    expect(await findByText('body of $1')).toBeTruthy()

    expect(
      container
        .querySelector('li.event-row[data-event-id="$2"]')!
        .querySelector('.reaction-picker'),
    ).not.toBeNull()
    expect(
      container
        .querySelector('li.event-row[data-event-id="$1"]')!
        .querySelector('.reaction-picker'),
    ).toBeNull()
  })

  it('opens the reaction picker below the target body and scrolls it into view', async () => {
    const scrollIntoView = vi.fn()
    Element.prototype.scrollIntoView = scrollIntoView
    const { findByRole, container } = renderRoom([event('$1', T0)])

    fireEvent.click(await findByRole('button', { name: 'React' }))

    const row = container.querySelector('li.event-row[data-event-id="$1"]')!
    const body = row.querySelector('.event-body')!
    const picker = row.querySelector('.reaction-picker-shell')!
    expect(
      body.compareDocumentPosition(picker) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).not.toBe(0)
    expect(scrollIntoView).toHaveBeenCalledWith({
      block: 'nearest',
      inline: 'nearest',
    })
  })

  it('?event= deep link jumps to the event and highlights it', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$old', T0 - 5 * DAY) }),
      ),
    )
    let seenAtTs: string | null = null
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(TIMELINE_PATH, ({ request }) => {
        seenAtTs = new URL(request.url).searchParams.get('at_ts')
        return HttpResponse.json({
          data: {
            events: [event('$old', T0 - 5 * DAY)],
            next_cursor: null,
          },
        })
      }),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24old`,
    )
    const services = testServices()
    const { findByText, container } = render(routedRoomPage(services))

    expect(await findByText('body of $old')).toBeTruthy()
    expect(seenAtTs).toBe(String(T0 - 5 * DAY))
    await waitFor(() =>
      expect(container.querySelector('.event-row.highlighted')).toBeTruthy(),
    )
  })

  it('a deep link scrolls the highlighted row into view (WCR-09)', async () => {
    const scrolled: { element: Element; options: ScrollIntoViewOptions }[] = []
    // jsdom has no scrollIntoView; install one so the reveal is observable.
    Element.prototype.scrollIntoView = function (options) {
      scrolled.push({
        element: this,
        options: options as ScrollIntoViewOptions,
      })
    }
    try {
      server.use(
        http.get(
          `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
          () => HttpResponse.json({ data: event('$old', T0 - 5 * DAY) }),
        ),
        http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
          HttpResponse.json({ data: [] }),
        ),
        http.get(TIMELINE_PATH, () =>
          HttpResponse.json({
            data: {
              events: [event('$newer', T0), event('$old', T0 - 5 * DAY)],
              next_cursor: null,
            },
          }),
        ),
      )
      window.history.replaceState(
        null,
        '',
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24old`,
      )
      const services = testServices()
      const { findByText } = render(routedRoomPage(services))

      await findByText('body of $old')
      await waitFor(() =>
        expect(
          scrolled.some(
            ({ element, options }) =>
              element.getAttribute('data-event-id') === '$old' &&
              options.block === 'center' &&
              options.inline === 'nearest',
          ),
        ).toBe(true),
      )
    } finally {
      delete (Element.prototype as { scrollIntoView?: unknown }).scrollIntoView
    }
  })

  it('a ?event= deep link into a thread reply opens the thread instead of jumping the room stream', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({
          data: event('$reply', T0, {
            relates_to: { rel_type: 'm.thread', event_id: '$root' },
          }),
        }),
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({ data: { events: [], next_cursor: null } }),
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24reply`,
    )
    const services = testServices()
    render(routedRoomPage(services))

    await waitFor(() =>
      expect(window.location.search).toBe('?thread=%24root&event=%24reply'),
    )
  })

  it('a dead-anchor lookup cannot mutate an unrelated page after RoomPage unmounts', async () => {
    let releaseLookup!: () => void
    const heldLookup = new Promise<void>((resolve) => {
      releaseLookup = resolve
    })
    let lookupStarted = false
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        async () => {
          lookupStarted = true
          await heldLookup
          return new HttpResponse(null, { status: 404 })
        },
      ),
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: {
            events: [event('$tail-after-away', T0)],
            next_cursor: null,
          },
        }),
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24gone`,
    )
    const { findByText } = render(routedRoomPageWithAwayRoute(testServices()))
    await waitFor(() => expect(lookupStarted).toBe(true))

    window.history.pushState(null, '', '/away?event=%24belongs-to-away')
    window.dispatchEvent(new PopStateEvent('popstate'))
    await findByText('Unrelated page')

    releaseLookup()
    await new Promise((resolve) => setTimeout(resolve, 80))

    expect(window.location.pathname).toBe('/away')
    expect(window.location.search).toBe('?event=%24belongs-to-away')
  })

  it('a stale thread lookup cannot navigate back after RoomPage unmounts', async () => {
    let releaseLookup!: () => void
    const heldLookup = new Promise<void>((resolve) => {
      releaseLookup = resolve
    })
    let lookupStarted = false
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        async () => {
          lookupStarted = true
          await heldLookup
          return HttpResponse.json({
            data: event('$reply-after-away', T0, {
              relates_to: { rel_type: 'm.thread', event_id: '$root' },
            }),
          })
        },
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24reply-after-away`,
    )
    const { findByText } = render(routedRoomPageWithAwayRoute(testServices()))
    await waitFor(() => expect(lookupStarted).toBe(true))

    window.history.pushState(null, '', '/away?event=%24belongs-to-away')
    window.dispatchEvent(new PopStateEvent('popstate'))
    await findByText('Unrelated page')

    releaseLookup()
    await new Promise((resolve) => setTimeout(resolve, 80))

    expect(window.location.pathname).toBe('/away')
    expect(window.location.search).toBe('?event=%24belongs-to-away')
  })

  it('handles a failed anchor-route replacement without an unhandled rejection', async () => {
    let releaseHead!: () => void
    const heldHead = new Promise<void>((resolve) => {
      releaseHead = resolve
    })
    let headStarted = false
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        () => new HttpResponse(null, { status: 404 }),
      ),
      http.get(TIMELINE_PATH, async () => {
        headStarted = true
        await heldHead
        return HttpResponse.json({
          data: {
            events: [event('$tail-route-error', T0)],
            next_cursor: null,
          },
        })
      }),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24gone`,
    )
    render(routedRoomPage(testServices()))
    await waitFor(() => expect(headStarted).toBe(true))
    const replaceState = vi
      .spyOn(window.history, 'replaceState')
      .mockImplementation(() => {
        throw new DOMException('rate limited', 'SecurityError')
      })

    releaseHead()
    await waitFor(() => expect(replaceState).toHaveBeenCalled())

    expect(window.location.search).toContain('event=%24gone')
  })

  it('keeps a search/deep-link target centered when timeline rows resize', async () => {
    installResizeObserver()
    const scrolled: string[] = []
    Element.prototype.scrollIntoView = function () {
      const id = this.getAttribute('data-event-id')
      if (id !== null) {
        scrolled.push(id)
      }
    }
    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$old', T0 - 5 * DAY) }),
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: {
            events: [event('$newer', T0), event('$old', T0 - 5 * DAY)],
            next_cursor: null,
          },
        }),
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24old`,
    )
    const { findByText } = render(routedRoomPage(testServices()))

    await findByText('body of $old')
    await waitFor(() => expect(scrolled).toContain('$old'))
    scrolled.length = 0

    await triggerObservedResizeFrame()

    expect(scrolled).toContain('$old')
  })

  it('stops keeping a search/deep-link target centered after the user scrolls', async () => {
    installResizeObserver()
    const scrolled: string[] = []
    Element.prototype.scrollIntoView = function () {
      const id = this.getAttribute('data-event-id')
      if (id !== null) {
        scrolled.push(id)
      }
    }
    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$old', T0 - 5 * DAY) }),
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: {
            events: [event('$newer', T0), event('$old', T0 - 5 * DAY)],
            next_cursor: null,
          },
        }),
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24old`,
    )
    const { container, findByText } = render(routedRoomPage(testServices()))

    await findByText('body of $old')
    await waitFor(() => expect(scrolled).toContain('$old'))
    scrolled.length = 0

    fireEvent.wheel(container.querySelector<HTMLElement>('.timeline')!)
    await triggerObservedResizeFrame()

    expect(scrolled).not.toContain('$old')
  })

  it('navigating to ?event= inside an open room jumps without a remount (WCR-09)', async () => {
    const atTsSeen: (string | null)[] = []
    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$old', T0 - 5 * DAY) }),
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(TIMELINE_PATH, ({ request }) => {
        const atTs = new URL(request.url).searchParams.get('at_ts')
        atTsSeen.push(atTs)
        return HttpResponse.json({
          data:
            atTs === null
              ? { events: [event('$new', T0)], next_cursor: 'c1' }
              : {
                  events: [event('$old', T0 - 5 * DAY)],
                  next_cursor: null,
                },
        })
      }),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
    const services = testServices()
    const { findByText, container } = render(routedRoomPage(services))
    await findByText('body of $new')

    // The user follows an in-room deep link (M-W10 search will route this
    // way): same room, new ?event= — previously ignored because the load
    // effect keyed on the timeline instance alone.
    window.history.pushState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24old`,
    )
    window.dispatchEvent(new PopStateEvent('popstate'))

    expect(await findByText('body of $old')).toBeTruthy()
    expect(atTsSeen).toContain(String(T0 - 5 * DAY))
    await waitFor(() =>
      expect(
        container
          .querySelector('.event-row.highlighted')
          ?.getAttribute('data-event-id'),
      ).toBe('$old'),
    )
  })

  it('a jumped timeline offers Load newer until it reaches the present', async () => {
    const all = [
      event('$old', T0 - 5 * DAY),
      event('$mid', T0 - 2 * DAY),
      event('$new', T0),
    ]
    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$old', T0 - 5 * DAY) }),
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      // The real endpoint's shape: newest-first, bounded from above by at_ts.
      http.get(TIMELINE_PATH, ({ request }) => {
        const params = new URL(request.url).searchParams
        const atTs = params.get('at_ts')
        let pool = [...all].sort((a, b) => b.origin_ts - a.origin_ts)
        if (atTs !== null) {
          pool = pool.filter((e) => e.origin_ts <= Number(atTs))
        }
        return HttpResponse.json({
          data: { events: pool, next_cursor: null },
        })
      }),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24old`,
    )
    const { findByText, findByRole, queryByRole, queryByText } = render(
      routedRoomPage(testServices()),
    )
    await findByText('body of $old')

    // Parked in history: the newest message is not loaded, and the timeline
    // offers the way forward (jsdom has no IntersectionObserver, so the
    // button is the testable path — same contract as "Load older").
    expect(queryByText('body of $new')).toBeNull()
    fireEvent.click(await findByRole('button', { name: 'Load newer messages' }))
    await findByText('body of $new')

    // Caught up: an empty probe retires the affordance.
    fireEvent.click(await findByRole('button', { name: 'Load newer messages' }))
    await waitFor(() =>
      expect(queryByRole('button', { name: 'Load newer messages' })).toBeNull(),
    )
  })

  it('opening the room clears its unread count', async () => {
    const { services } = renderRoom([event('$1', T0)])
    services.rooms.noteUnreadCounts(ACCOUNT, ROOM, 3, 0)
    // The first mount already cleared the summary count, so re-assert via a
    // fresh mount instead: the effect must clear pre-existing counts.
    cleanup()
    const key = `${ACCOUNT}/${ROOM}`
    expect(services.rooms.unreadCount(key)).toBe(3)

    render(routedRoomPage(services))
    await waitFor(() => expect(services.rooms.unreadCount(key)).toBe(0))
  })

  it('opening the room advances the read marker from the room summary', async () => {
    // The summary event is in the slice, so this view can tell it is an ordinary
    // main-timeline message rather than a thread reply. ADR 0096 made that a
    // precondition: seeding a read position from an event the client cannot
    // classify is what parked the marker on a thread member and made a room
    // report no unread thread while badging for one (#207/#209). The fixture was
    // an empty timeline before that; the effect now stands down there and the
    // timeline effect owns the marker.
    const { services } = renderRoom([event('$last', T0)])

    await waitFor(() =>
      expect(services.deviceState.readMarker(ACCOUNT, ROOM)).toEqual({
        eventId: '$last',
        originTs: T0,
      }),
    )
  })

  it('does not advance the read marker from a summary event it cannot classify', async () => {
    // `last_event_id` is `MAX(origin_ts)` over every event including thread
    // replies, and `rooms`/`timeline` update independently — so "absent from the
    // slice" cannot be read as "not a thread reply" (ADR 0096).
    const { services } = renderRoom([])

    await new Promise((resolve) => setTimeout(resolve, 100))
    expect(services.deviceState.readMarker(ACCOUNT, ROOM)).toBeNull()
  })

  describe('a view parked in history claims nothing as read', () => {
    /**
     * A `?event=$old` landing five days back, with `$new` at the present and a
     * room summary whose `last_event_id` is newer still — so both the
     * optimistic clear and the summary-derived marker have something they
     * *could* wrongly claim, and the assertions below are not vacuous.
     */
    function historyJump(tag: string) {
      // Unique ids per test: a sibling test's debounced receipt can fire inside
      // this one's window (the sender's timer outlives its own `cleanup()`),
      // and identical fixtures would make the two indistinguishable.
      const old = `$old-${tag}`
      const fresh = `$new-${tag}`
      const all = [
        event(old, T0 - 5 * DAY, { body: `body of ${old}` }),
        event(`$mid-${tag}`, T0 - 2 * DAY, { body: 'body of $mid' }),
        event(fresh, T0, { body: `body of ${fresh}` }),
      ]
      const reads: string[] = []
      server.use(
        http.post(
          `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/read`,
          async ({ request }) => {
            const body = (await request.json()) as { event_id: string }
            reads.push(body.event_id)
            return HttpResponse.json({ data: {} })
          },
        ),
        http.get(
          `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
          () => HttpResponse.json({ data: all[0] }),
        ),
        http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
          HttpResponse.json({
            data: [
              {
                account_id: ACCOUNT,
                account_user_id: '@me:hs',
                room_id: ROOM,
                name: 'Ops',
                topic: null,
                avatar_url: null,
                canonical_alias: null,
                last_activity_ts: T0,
                last_event_id: '$last',
              },
            ],
          }),
        ),
        http.get(TIMELINE_PATH, ({ request }) => {
          const atTs = new URL(request.url).searchParams.get('at_ts')
          let pool = [...all].sort((a, b) => b.origin_ts - a.origin_ts)
          if (atTs !== null) {
            pool = pool.filter((e) => e.origin_ts <= Number(atTs))
          }
          return HttpResponse.json({
            data: { events: pool, next_cursor: null },
          })
        }),
      )
      window.history.replaceState(
        null,
        '',
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=${encodeURIComponent(old)}`,
      )
      const services = testServices()
      // Spy rather than seed-and-read: the optimistic clear races the room-list
      // fetch that would repopulate the count, so the call itself is the
      // deterministic signal.
      const cleared = vi.spyOn(services.rooms, 'noteUnreadCounts')
      const mine = () => reads.filter((id) => id === old || id === fresh)
      return {
        services,
        reads,
        cleared,
        mine,
        anchor: old,
        newest: fresh,
        ...render(routedRoomPage(services)),
      }
    }

    it('does not clear the unread count on an anchored load', async () => {
      const { cleared, anchor, findByText } = historyJump('count')
      await findByText(`body of ${anchor}`)

      // The badge belongs to `$new`, which this view has never shown. Clearing
      // it here is the bug: it hides the room for the session and the count
      // returns on the next load, since the server correctly refuses to
      // advance the receipt past what was displayed (ADR 0089).
      expect(cleared).not.toHaveBeenCalledWith(ACCOUNT, ROOM, 0, 0)
    })

    it('still clears optimistically on an ordinary open', async () => {
      const { services } = renderRoom([event('$1', T0)])
      const cleared = vi.spyOn(services.rooms, 'noteUnreadCounts')
      cleanup()

      render(routedRoomPage(services))
      await waitFor(() =>
        expect(cleared).toHaveBeenCalledWith(ACCOUNT, ROOM, 0, 0),
      )
    })

    it('does not advance the cross-device read marker on an anchored load', async () => {
      const { services, anchor, findByText } = historyJump('marker')
      await findByText(`body of ${anchor}`)

      // A sibling device turns this marker straight into a zeroed badge
      // (`connectReadMarkers`), so jumping it to the summary's `$last` would
      // mark the room read on every device from a view parked in history.
      expect(services.deviceState.readMarker(ACCOUNT, ROOM)).toBeNull()
    })

    it('sends no receipt while the view stays anchored in history', async () => {
      const { mine, anchor, findByText } = historyJump('receipt')
      await findByText(`body of ${anchor}`)
      // Past the sender's own debounce, so "nothing sent" means nothing was
      // going to be sent — not that the timer hasn't fired yet.
      await new Promise((resolve) =>
        setTimeout(resolve, RECEIPT_DEBOUNCE_MS + 150),
      )
      // Nothing, even though the head has been gap-filled behind the anchor —
      // which is why `atEnd` alone can't gate this.
      expect(mine()).toEqual([])
    })
  })

  describe('the receipt names the arrival-newest event (ADR 0089)', () => {
    /**
     * The LinkedIn portal that prompted ADR 0089, with its real numbers. A
     * mautrix bridge creates the room, emits its own state, and *then* backfills
     * the pre-existing conversation carrying its real, older timestamps — so the
     * backfilled message is oldest by `origin_ts` and newest by arrival order:
     *
     *   event                  origin_ts        arrival_order   rendered?
     *   m.room.create          1785928306622    1871406         no (state)
     *   uk.half-shot.bridge    1785928309453    1871424         no (state)
     *   m.room.message  (mid)  1785928308453    1871410         yes
     *   m.room.message  (old)  1785928304987    1871426         yes
     *
     * Three separate things have to hold at once here, which is why one fixture
     * carries all of them:
     *
     *  - the receipt names the **arrival-max** visible event (the backfilled
     *    `old` message), not the display-last one — the original bug;
     *  - the marker names the **display-last** visible event (`mid`), on
     *    `origin_ts` — handing it the arrival-newest event would feed it an
     *    older timestamp than it holds and, being forward-only, it would stop
     *    advancing altogether;
     *  - **neither** names a state event the user never saw rendered. Under the
     *    default `stateEvents: 'important'` the create and bridge events do not
     *    render, and ADR 0089's contract is "among the events it has actually
     *    displayed". `bridgeArrival` lets a test hand the hidden bridge event
     *    the winning arrival order to prove the filter is what excludes it,
     *    rather than the comparison happening to favour a visible event.
     */
    const CREATE_TS = 1_785_928_306_622
    const BRIDGE_TS = 1_785_928_309_453
    const MID_TS = 1_785_928_308_453
    const MESSAGE_TS = 1_785_928_304_987

    function portal(tag: string, bridgeArrival = 1_871_424) {
      // Unique ids per test: the ephemeral sender's debounce timer outlives its
      // own `cleanup()`, so a sibling test's receipt can land inside this one's
      // window and identical fixtures would be indistinguishable.
      const ids = {
        create: `$create-${tag}`,
        bridge: `$bridge-${tag}`,
        mid: `$mid-${tag}`,
        message: `$message-${tag}`,
      }
      const all = [
        event(ids.create, CREATE_TS, {
          type: 'm.room.create',
          state_key: '',
          arrival_order: 1_871_406,
        }),
        event(ids.bridge, BRIDGE_TS, {
          type: 'uk.half-shot.bridge',
          state_key: 'linkedin',
          arrival_order: bridgeArrival,
        }),
        event(ids.mid, MID_TS, {
          arrival_order: 1_871_410,
          body: 'body of $mid',
          content: { msgtype: 'm.text', body: 'body of $mid' },
        }),
        event(ids.message, MESSAGE_TS, {
          arrival_order: 1_871_426,
          body: 'body of $message',
          content: { msgtype: 'm.text', body: 'body of $message' },
        }),
      ]
      const reads: string[] = []
      server.use(
        http.post(
          `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/read`,
          async ({ request }) => {
            const body = (await request.json()) as { event_id: string }
            reads.push(body.event_id)
            return HttpResponse.json({ data: {} })
          },
        ),
      )
      // The endpoint's shape is newest-first in display order, which the store
      // reverses. No room summary: the summary-derived marker is a separate
      // effect on a separate input, and leaving it out keeps this test about the
      // timeline effect's two picks.
      const rendered = renderRoom(
        [...all].sort((a, b) => b.origin_ts - a.origin_ts),
        { rooms: [] },
      )
      const ours = Object.values(ids)
      return {
        ...rendered,
        ids,
        mine: () => reads.filter((id) => ours.includes(id)),
      }
    }

    it('receipts the backfilled message, not the display-last state event', async () => {
      const { ids, mine, findByText } = portal('receipt')
      await findByText('body of $message')

      // Past the sender's debounce, so this is the settled choice.
      await waitFor(() => expect(mine()).toEqual([ids.message]))
      // Spelled out because it is the entire bug: the display-last event is the
      // bridge state event, and that is what the old `findLast` sent.
      expect(mine()).not.toContain(ids.bridge)
    })

    it('still advances the cross-device marker in display order', async () => {
      const { services, ids, findByText } = portal('marker')
      await findByText('body of $message')

      // The marker is a display-order artifact (ADR 0048) and stays on
      // `origin_ts`: it names the *display-last* event, not the receipt's.
      // Handing it the arrival-newest event would feed it an older `origin_ts`
      // than it already holds and — being forward-only — it would stop advancing
      // altogether.
      await waitFor(() =>
        expect(services.deviceState.readMarker(ACCOUNT, ROOM)).toEqual({
          eventId: ids.mid,
          originTs: MID_TS,
        }),
      )
    })

    it('recomputes both picks when the visibility rule changes mid-room', async () => {
      // The predicate is re-created every render and so cannot be a dependency
      // itself; the values it closes over have to be listed instead. Without
      // them, turning state events on repaints the timeline while both read
      // positions stay computed under the old rule — visibly inconsistent with
      // what is on screen, and stuck that way until some unrelated dependency
      // happens to fire the effect.
      const { services, ids, mine, findByText } = portal('toggle', 1_871_999)
      await findByText('body of $message')
      await waitFor(() => expect(mine()).toEqual([ids.message]))

      // The bridge event becomes a rendered row, and it is both display-last
      // and arrival-max — so it is now the correct answer to both picks.
      services.settings.stateEvents.value = 'all'

      await waitFor(() => expect(mine().at(-1)).toBe(ids.bridge))
      await waitFor(() =>
        expect(services.deviceState.readMarker(ACCOUNT, ROOM)).toEqual({
          eventId: ids.bridge,
          originTs: BRIDGE_TS,
        }),
      )
    })

    it('names no event the user never saw, on either pick', async () => {
      // The bridge event now wins *both* comparisons outright — it is
      // display-last by `origin_ts` and, at 1_871_999, arrival-max as well. It
      // is also a state event the default `stateEvents: 'important'` does not
      // render. Only the visibility filter can keep it out of both picks, so
      // this fails the moment that filter goes away, in a way the numbers alone
      // cannot rescue.
      const { services, ids, mine, findByText } = portal('hidden', 1_871_999)
      await findByText('body of $message')

      await waitFor(() => expect(mine()).toEqual([ids.message]))
      expect(mine()).not.toContain(ids.bridge)
      expect(services.deviceState.readMarker(ACCOUNT, ROOM)).toEqual({
        eventId: ids.mid,
        originTs: MID_TS,
      })
    })
  })

  /// A `?event=` target the server cannot resolve leaves the view on the live
  /// tail. The anchor must not linger: while it does, the read-state gates all
  /// treat the view as parked in history and the room can never be marked read
  /// (PR review on #136).
  it('drops an unresolvable ?event= anchor and then claims the room read', async () => {
    const reads: string[] = []
    server.use(
      // The deep-link target is gone (redacted, purged, or never ours).
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        () => new HttpResponse(null, { status: 404 }),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/read`,
        async ({ request }) => {
          const body = (await request.json()) as { event_id: string }
          reads.push(body.event_id)
          return HttpResponse.json({ data: {} })
        },
      ),
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: {
            events: [event('$tail-dead-anchor', T0, { body: 'body of $tail' })],
            next_cursor: null,
          },
        }),
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24gone&thread=%24keep`,
    )
    const { findByText } = render(routedRoomPage(testServices()))
    await findByText('body of $tail')

    // The dead anchor is dropped; unrelated params are left alone.
    await waitFor(() => expect(window.location.search).not.toContain('event='))
    expect(window.location.search).toContain('thread=%24keep')

    // And the view is now treated as the live-tail view it actually is.
    await waitFor(() =>
      expect(reads.filter((id) => id === '$tail-dead-anchor')).toEqual([
        '$tail-dead-anchor',
      ]),
    )
  })

  /// The page does not remount across an in-room navigation (ADR 0085), so a
  /// slow lookup for anchor A can resolve *after* the user has followed a second
  /// deep link to B. The stale continuation must not strip B's anchor off the
  /// URL it now finds there.
  it('a stale anchor lookup does not strip a newer anchor', async () => {
    let releaseFirstLookup: () => void = () => {}
    const firstLookupHeld = new Promise<void>((resolve) => {
      releaseFirstLookup = resolve
    })
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        async ({ params }) => {
          // Correct whether or not msw has already decoded the path param.
          if (decodeURIComponent(String(params.eventId)) === '$slow') {
            // Held until the user has already navigated away to `$b`.
            await firstLookupHeld
            return new HttpResponse(null, { status: 404 })
          }
          return HttpResponse.json({ data: event('$b', T0 - DAY) })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: {
            events: [event('$b', T0 - DAY, { body: 'body of $b' })],
            next_cursor: null,
          },
        }),
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24slow`,
    )
    const { findByText } = render(routedRoomPage(testServices()))

    // The user follows a second deep link before the first lookup answers.
    window.history.pushState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24b`,
    )
    window.dispatchEvent(new PopStateEvent('popstate'))
    await findByText('body of $b')

    // Now let the abandoned lookup finish and fail.
    releaseFirstLookup()
    await new Promise((resolve) => setTimeout(resolve, 80))

    expect(window.location.search).toContain('event=%24b')
  })

  /// `refreshHead` deliberately no-ops on a slice parked in history that shares
  /// nothing with the head, without setting an error. Reading that untouched
  /// slice as proof of absence would strip the anchor while the view is still
  /// showing history — and then claim read state from it.
  it('keeps a ?event= anchor when the head load declines to move a parked slice', async () => {
    const parked = event('$old', T0 - 5 * DAY, { body: 'body of $old' })
    const head = event('$new', T0, { body: 'body of $new' })
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        ({ params }) =>
          decodeURIComponent(String(params.eventId)) === '$old'
            ? HttpResponse.json({ data: parked })
            : new HttpResponse(null, { status: 404 }),
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      // Head and parked page share no events, so `refreshHead` bails.
      http.get(TIMELINE_PATH, ({ request }) => {
        const atTs = new URL(request.url).searchParams.get('at_ts')
        return HttpResponse.json({
          data:
            atTs === null
              ? { events: [head], next_cursor: 'c1' }
              : { events: [parked], next_cursor: 'c2' },
        })
      }),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24old`,
    )
    const { findByText } = render(routedRoomPage(testServices()))
    await findByText('body of $old')

    // A second, dead deep link while parked in history.
    window.history.pushState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24dead`,
    )
    window.dispatchEvent(new PopStateEvent('popstate'))
    await new Promise((resolve) => setTimeout(resolve, 80))

    expect(window.location.search).toContain('event=%24dead')
  })

  /// A room id reached by cold navigation or an external deep link keeps its
  /// literal `:` in `location.pathname`, which no amount of `encodeURIComponent`
  /// on our side will match. The staleness check must not depend on that.
  it('drops a dead anchor on a URL with an unencoded room id', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        () => new HttpResponse(null, { status: 404 }),
      ),
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: {
            events: [event('$tail-raw', T0, { body: 'body of $tail-raw' })],
            next_cursor: null,
          },
        }),
      ),
    )
    // Note: no `encodeURIComponent` — this is what a browser actually shows.
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${ROOM}?event=%24gone`,
    )
    const { findByText } = render(routedRoomPage(testServices()))
    await findByText('body of $tail-raw')

    await waitFor(() => expect(window.location.search).not.toContain('event='))
  })

  /// `refreshHead`'s wholesale-replace branch discards the outgoing slice when
  /// the fresh head shares nothing with it. If the anchor's target arrived in
  /// that slice while the by-id lookup was in flight, refreshing to "check"
  /// evicts the proof that the event exists — so a target already loaded is
  /// answer enough and the refresh must not run at all.
  it('keeps a ?event= anchor whose target arrived while the lookup was in flight', async () => {
    let release404!: () => void
    const held = new Promise<void>((resolve) => {
      release404 = resolve
    })
    let headCalls = 0
    let byIdCalls = 0
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        async () => {
          byIdCalls += 1
          await held
          return new HttpResponse(null, { status: 404 })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(TIMELINE_PATH, () => {
        headCalls += 1
        return HttpResponse.json({
          data:
            headCalls === 1
              ? { events: [event('$seed', T0 - DAY)], next_cursor: null }
              : // Any later head shares nothing with the slice — the eviction
                // branch.
                { events: [event('$fresh', T0 + 10)], next_cursor: null },
        })
      }),
    )
    // Cold, unanchored: seeds a slice at the live end without the target.
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
    const services = testServices()
    const { findByText } = render(routedRoomPage(services))
    await findByText('body of $seed')

    // Anchor on an event the slice does not hold: the by-id lookup fires, and
    // is held open.
    window.history.pushState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24late`,
    )
    window.dispatchEvent(new PopStateEvent('popstate'))
    // Let the effect actually issue the lookup before anything else happens —
    // otherwise the live event below lands first, the effect finds the target
    // already loaded, and never asks.
    await waitFor(() => expect(byIdCalls).toBe(1))

    // While it is in flight the event arrives live, so the anchor is now
    // satisfiable — the room can highlight it.
    services.live.start()
    services.sockets[0].emitOpen()
    services.sockets[0].emitMessage(
      JSON.stringify({
        type: 'timeline.event',
        account_id: ACCOUNT,
        payload: event('$late', T0, { body: 'body of $late' }),
      }),
    )
    await findByText('body of $late')

    // Only now does the lookup answer 404.
    release404()
    await new Promise((resolve) => setTimeout(resolve, 80))

    expect(window.location.search).toContain('event=%24late')
  })

  /// A superseded head load means a sibling load won the generation race. If
  /// that winner loaded the anchor, retrying without checking the winner's
  /// slice can immediately evict the proof that the permalink is valid.
  it('keeps a ?event= anchor loaded by the call that superseded its head check', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        () => new HttpResponse(null, { status: 404 }),
      ),
    )
    const services = testServices()
    const timeline = services.timelines.acquire(ACCOUNT, ROOM)
    const loadLatest = vi
      .spyOn(timeline, 'loadLatest')
      .mockImplementationOnce(async () => {
        // The sibling winner loaded the anchor into the shared store.
        timeline.ingestLive(event('$race-winner', T0))
        return 'superseded'
      })
      .mockImplementationOnce(async () => {
        // A disjoint retry models refreshHead's wholesale-replace branch.
        timeline.resumeAtHead()
        return 'applied'
      })
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24race-winner`,
    )

    render(routedRoomPage(services))
    await waitFor(() => expect(loadLatest).toHaveBeenCalled())
    await new Promise((resolve) => setTimeout(resolve, 80))

    expect(loadLatest).toHaveBeenCalledTimes(1)
    expect(window.location.search).toContain('event=%24race-winner')
  })

  /// The timeline store is keyed by account *and* room (ADR 0085), two of the
  /// user's accounts can be in the same room, and the page does not remount when
  /// the account changes under the same room and anchor. A stale continuation
  /// from the old account must not act on the new account's URL.
  it('a stale lookup from another account does not strip the current anchor', async () => {
    const OTHER = '6b53f7f0-0000-4000-8000-000000000002'
    let releaseFirst!: () => void
    const firstHeld = new Promise<void>((resolve) => {
      releaseFirst = resolve
    })
    server.use(
      // Account A's lookup hangs, then 404s; account B's resolves fine.
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        async () => {
          await firstHeld
          return new HttpResponse(null, { status: 404 })
        },
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/${OTHER}/events/:eventId`, () =>
        HttpResponse.json({ data: event('$shared', T0 - DAY) }),
      ),
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({ data: [] }),
      ),
      // The two accounts see different rooms' worth of history: A cannot see
      // `$shared` at all, which is why its lookup 404s and why its slice can
      // never corroborate B's anchor.
      http.get(
        `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/timeline`,
        ({ params }) =>
          HttpResponse.json({
            data: {
              events: [
                params.accountId === OTHER
                  ? event('$shared', T0 - DAY, { body: 'body of $shared' })
                  : event('$a-only', T0 - DAY, { body: 'body of $a-only' }),
              ],
              next_cursor: null,
            },
          }),
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24shared`,
    )
    const { findAllByText } = render(routedRoomPage(testServices()))

    // Same room, same anchor, different account — no remount.
    window.history.pushState(
      null,
      '',
      `/${OTHER}/rooms/${encodeURIComponent(ROOM)}?event=%24shared`,
    )
    window.dispatchEvent(new PopStateEvent('popstate'))
    await findAllByText('body of $shared')

    // Account A's lookup finally fails. It says nothing about account B's view.
    releaseFirst()
    await new Promise((resolve) => setTimeout(resolve, 80))

    expect(window.location.pathname).toContain(OTHER)
    expect(window.location.search).toContain('event=%24shared')
  })

  /// A lookup that fails for any reason *other* than "no such event" proves
  /// nothing about the anchor. Dropping it there would let a transient blip
  /// permanently destroy a permalink — and mark the room read on the way out.
  it('keeps a ?event= anchor when the lookup fails transiently', async () => {
    for (const [label, handler] of [
      ['5xx', () => new HttpResponse(null, { status: 503 })],
      ['network error', () => HttpResponse.error()],
    ] as const) {
      server.use(
        http.get(
          `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
          handler,
        ),
        http.get(TIMELINE_PATH, () =>
          HttpResponse.json({
            data: {
              events: [event('$tail', T0, { body: `tail for ${label}` })],
              next_cursor: null,
            },
          }),
        ),
      )
      window.history.replaceState(
        null,
        '',
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24missing`,
      )
      const { findByText } = render(routedRoomPage(testServices()))
      await findByText(`tail for ${label}`)
      await new Promise((resolve) => setTimeout(resolve, 50))

      expect(window.location.search, label).toContain('event=%24missing')
      cleanup()
    }
  })

  /// "Not in the slice" only means "absent" if the slice is trustworthy. When
  /// the head load itself fails there is nothing to conclude from, and the
  /// anchor must survive for a retry.
  it('keeps a ?event= anchor when the head load fails', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        () => new HttpResponse(null, { status: 404 }),
      ),
      http.get(TIMELINE_PATH, () => new HttpResponse(null, { status: 500 })),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24missing`,
    )
    render(routedRoomPage(testServices()))
    await waitFor(() => expect(window.location.search).toBeTruthy())
    await new Promise((resolve) => setTimeout(resolve, 80))

    expect(window.location.search).toContain('event=%24missing')
  })

  /// The by-id lookup 404ing does not mean the anchor is unsatisfiable — the
  /// server may simply not serve that event by id while the event is right there
  /// in the room (the e2e mock does exactly this for seeded history). Dropping
  /// the anchor then would throw away the highlight the deep link exists for.
  it('keeps a ?event= anchor whose target is in the loaded tail', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        () => new HttpResponse(null, { status: 404 }),
      ),
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: {
            events: [event('$in-tail', T0, { body: 'body of $in-tail' })],
            next_cursor: null,
          },
        }),
      ),
    )
    window.history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}?event=%24in-tail`,
    )
    const { findByText } = render(routedRoomPage(testServices()))
    await findByText('body of $in-tail')

    // Give the fallback time to settle, then confirm the anchor survived.
    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(window.location.search).toContain('event=%24in-tail')
  })

  // ADR 0085 phase 1: the store survives the room switch, so re-entry has
  // something to paint and reconciles it in place.
  describe('re-entering a room in one session (ADR 0085 phase 1)', () => {
    const OTHER_ROOM = '!other:hs'

    /**
     * Render at room A, switch to B, come back — optionally through a
     * deep link. `heads` are A's successive head responses.
     */
    async function roundTrip(
      heads: (() => Response | Promise<Response>)[],
      backQuery = '',
      /**
       * The page a `?event=` jump lands on. A single responder, not a
       * sequence: late `at_ts` probes leaked by other tests (see below) would
       * otherwise consume entries and make this depend on test order.
       */
      jump?: () => Response,
    ) {
      let head = 0
      server.use(
        http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
          HttpResponse.json({ data: [] }),
        ),
        http.get(
          `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/timeline`,
          ({ params, request }) => {
            if (decodeURIComponent(String(params.roomId)) === OTHER_ROOM) {
              return HttpResponse.json({
                data: { events: [event('$b', T0)], next_cursor: null },
              })
            }
            // Head fetches only. An `at_ts` probe here belongs to a
            // `loadNewer` chain another test in this file left in flight —
            // `server.resetHandlers()` sends its late requests to whichever
            // handler is registered next — and counting those would make this
            // test's response sequence depend on test order.
            if (new URL(request.url).searchParams.has('at_ts')) {
              return (
                jump?.() ??
                HttpResponse.json({
                  data: { events: [], next_cursor: null },
                })
              )
            }
            const respond = heads[Math.min(head, heads.length - 1)]
            head += 1
            return respond()
          },
        ),
      )
      const go = (roomId: string, query = '') => {
        window.history.pushState(
          null,
          '',
          `/${ACCOUNT}/rooms/${encodeURIComponent(roomId)}${query}`,
        )
        window.dispatchEvent(new PopStateEvent('popstate'))
      }
      window.history.replaceState(
        null,
        '',
        `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
      )
      const utils = render(routedRoomPage(testServices()))
      await utils.findByText('body of $1')
      go(OTHER_ROOM)
      await utils.findByText('body of $b')
      go(ROOM, backQuery)
      return { ...utils, heads: () => head }
    }

    it('paints the warm slice first, then merges what arrived while away', async () => {
      // The re-entry head fetch is held open, so whatever is on screen before
      // it settles came from the warm store — before phase 1 this was
      // "Loading messages…" until the network answered.
      let settle: (response: Response) => void = () => {}
      const { findByText, queryByText, heads } = await roundTrip([
        () =>
          HttpResponse.json({
            data: { events: [event('$1', T0)], next_cursor: 'c1' },
          }),
        () =>
          new Promise<Response>((resolve) => {
            settle = resolve
          }),
      ])

      expect(await findByText('body of $1')).toBeTruthy()
      expect(queryByText('Loading messages…')).toBeNull()

      // The held request is in flight by now, so releasing it is not a race.
      await waitFor(() => expect(heads()).toBe(2))

      // Live frames only reach the mounted room (ADR 0061), so the warm slice
      // missed `$2`; the head fetch overlaps it and `refreshHead` merges.
      settle(
        HttpResponse.json({
          data: {
            events: [event('$2', T0 + 1000), event('$1', T0)],
            next_cursor: 'c1',
          },
        }) as Response,
      )

      expect(await findByText('body of $2')).toBeTruthy()
      // Merged, not replaced: the row already loaded survives the reconcile.
      expect(await findByText('body of $1')).toBeTruthy()
    })

    it('gap-fills on re-entry through a ?event= link the slice already holds', async () => {
      // The jump effect fetches nothing when its target is already loaded, so
      // on this path the warm slice has no other route back to the present.
      const { findByText, heads } = await roundTrip(
        [
          () =>
            HttpResponse.json({
              data: { events: [event('$1', T0)], next_cursor: 'c1' },
            }),
          () =>
            HttpResponse.json({
              data: {
                events: [event('$3', T0 + 1000), event('$1', T0)],
                next_cursor: 'c1',
              },
            }),
        ],
        '?event=%241',
      )

      expect(await findByText('body of $3')).toBeTruthy()
      expect(heads()).toBe(2)
    })

    it('lets a jump off the warm slice win the race with the gap-fill', async () => {
      // The contended case: the `?event=` target is *not* in the warm slice,
      // so re-entry runs both the gap-fill (`loadLatest` over a populated
      // slice is `refreshHead`) and the jump, against one store. The jump
      // parks the slice in history; a gap-fill that landed afterwards would
      // splice the present onto it and teleport the reader back to the
      // present — the WCR-05 bug. `sliceGeneration`, bumped by the jump and
      // rechecked by `refreshHead` after its fetch, is what stops it.
      //
      // The head deliberately *overlaps* the jump page. A disjoint one would
      // be refused for a parked slice whatever the generation said, and the
      // test would pass without exercising the guard at all.
      const JUMP_TS = T0 - 60_000
      let settle: (response: Response) => void = () => {}
      server.use(
        http.get(
          `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
          () => HttpResponse.json({ data: event('$jump', JUMP_TS) }),
        ),
      )
      const { findByText, queryByText } = await roundTrip(
        [
          () =>
            HttpResponse.json({
              data: { events: [event('$1', T0)], next_cursor: 'c1' },
            }),
          () =>
            new Promise<Response>((resolve) => {
              settle = resolve
            }),
        ],
        '?event=%24jump',
        () =>
          HttpResponse.json({
            data: { events: [event('$jump', JUMP_TS)], next_cursor: 'c0' },
          }) as Response,
      )

      expect(await findByText('body of $jump')).toBeTruthy()

      settle(
        HttpResponse.json({
          data: {
            events: [event('$new', T0 + 1000), event('$jump', JUMP_TS)],
            next_cursor: 'c1',
          },
        }) as Response,
      )

      // Settling is two microtask hops from being applied, so give the
      // discarded response every chance to land before asserting it did not.
      await waitFor(() => expect(queryByText('body of $new')).toBeNull())
      await new Promise((resolve) => setTimeout(resolve, 0))
      expect(queryByText('body of $new')).toBeNull()
      expect(await findByText('body of $jump')).toBeTruthy()
    })
  })
})
