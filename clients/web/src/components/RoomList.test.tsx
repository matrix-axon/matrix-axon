import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { options } from 'preact'
import { LocationProvider } from 'preact-iso'
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
import { roomKey } from '../stores/room-list'
import { TEST_BASE_URL, testServices } from '../test/services'
import { RoomList } from './RoomList'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const OTHER_ACCOUNT = '6b53f7f0-0000-4000-8000-000000000002'

function makeRoom(overrides: Record<string, unknown>): Record<string, unknown> {
  return {
    account_id: ACCOUNT,
    account_user_id: '@me:example.org',
    last_activity_ts: Date.now(),
    notification_count: 0,
    highlight_count: 0,
    ...overrides,
  }
}

const OPS = makeRoom({
  room_id: '!ops:hs',
  name: 'Ops',
  last_activity_ts: Date.now() - 60_000,
})
const LOUNGE = makeRoom({
  room_id: '!lounge:hs',
  canonical_alias: '#lounge:hs',
  last_activity_ts: Date.now() - 3_600_000,
})
const DM = makeRoom({ room_id: '!dm:hs' })
const OTHER = makeRoom({
  account_id: OTHER_ACCOUNT,
  account_user_id: '@work:example.org',
  room_id: '!work:hs',
  name: 'Work',
})

function makeAccount(
  accountId: string,
  userId: string,
): Record<string, unknown> {
  return {
    account_id: accountId,
    user_id: userId,
    homeserver_url: 'https://matrix.example.org',
    state: 'active',
    device_id: 'AXONWEB',
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
}

function activeAccountsForRooms(rooms: unknown[]): Record<string, unknown>[] {
  const byId = new Map<string, string>()
  for (const room of rooms) {
    if (
      typeof room === 'object' &&
      room !== null &&
      typeof (room as { account_id?: unknown }).account_id === 'string' &&
      typeof (room as { account_user_id?: unknown }).account_user_id ===
        'string'
    ) {
      byId.set(
        (room as { account_id: string }).account_id,
        (room as { account_user_id: string }).account_user_id,
      )
    }
  }
  return [...byId.entries()].map(([accountId, userId]) =>
    makeAccount(accountId, userId),
  )
}

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  vi.unstubAllGlobals()
})
afterAll(() => server.close())

function renderPage(
  rooms: unknown[] = [OPS, LOUNGE, DM, OTHER],
  configure?: (services: ReturnType<typeof testServices>) => void,
  options: {
    activeAccounts?: unknown[]
    readMarkers?: Record<string, unknown>
  } = {},
) {
  const activeAccounts = options.activeAccounts ?? activeAccountsForRooms(rooms)
  const readMarkers = options.readMarkers ?? {}
  server.use(
    http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
      HttpResponse.json({ data: activeAccounts }),
    ),
    http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
      HttpResponse.json({ data: rooms }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`,
      ({ params }) =>
        HttpResponse.json({
          data: {
            namespace: params.namespace,
            entries: params.namespace === 'read_markers' ? readMarkers : {},
          },
        }),
    ),
    http.put(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () =>
      HttpResponse.json({ data: { updated_at: '2026-07-20T12:00:00Z' } }),
    ),
    // DM title resolution for the unnamed room.
    http.get(
      `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/members`,
      () =>
        HttpResponse.json({
          data: [
            { user_id: '@me:example.org', membership: 'join' },
            {
              user_id: '@bob:example.org',
              membership: 'join',
              display_name: 'Bob',
            },
          ],
        }),
    ),
  )
  const services = testServices()
  configure?.(services)
  const utils = render(
    <ServicesContext.Provider value={services}>
      {/* RoomList reads useLocation for Ctrl-K / Ctrl-arrow navigation. */}
      <LocationProvider>
        <RoomList />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  return { services, ...utils }
}

function roomActionsDetails(container: ParentNode): HTMLDetailsElement {
  const details = container.querySelector('.room-actions-menu')
  if (!(details instanceof HTMLDetailsElement)) {
    throw new Error('expected room actions details menu')
  }
  return details
}

function roomActionsSummary(container: ParentNode): HTMLElement {
  const summary = roomActionsDetails(container).querySelector('summary')
  if (!(summary instanceof HTMLElement)) {
    throw new Error('expected room actions summary')
  }
  return summary
}

function roomPointerEvent(
  type: string,
  init: {
    pointerId: number
    pointerType: string
    clientX: number
    clientY: number
  },
): Event {
  const event = new MouseEvent(type, {
    bubbles: true,
    cancelable: true,
    clientX: init.clientX,
    clientY: init.clientY,
  })
  for (const [key, value] of Object.entries({
    pointerId: init.pointerId,
    pointerType: init.pointerType,
    clientX: init.clientX,
    clientY: init.clientY,
  })) {
    Object.defineProperty(event, key, { configurable: true, value })
  }
  return event
}

describe('RoomList', () => {
  it('lists rooms with member-derived DM titles and deep-link hrefs', async () => {
    const { findByText, container } = renderPage()

    expect(await findByText('Ops')).toBeTruthy()
    expect(await findByText('Bob')).toBeTruthy() // resolved DM title

    const links = [...container.querySelectorAll('a.room-link')].map((a) =>
      a.getAttribute('href'),
    )
    expect(links).toContain(
      `/${ACCOUNT}/rooms/${encodeURIComponent('!ops:hs')}`,
    )
  })

  it('offers room actions from the control menu', async () => {
    const { container, findByText, getByRole } = renderPage()

    expect(await findByText('Ops')).toBeTruthy()
    const summary = roomActionsSummary(container)
    expect(summary.getAttribute('aria-label')).toBe('Rooms')
    expect(summary.getAttribute('title')).toBe(
      'Room actions (+; /join, /dm, /create, /find)',
    )
    expect(summary.getAttribute('aria-keyshortcuts')).toBe('+')
    expect(summary.getAttribute('role')).toBeNull()
    expect(summary.textContent).toContain('Rooms')
    const actions = ['Join', 'DM', 'Create', 'Find'].map((name) =>
      getByRole('link', { name }),
    )
    expect(actions.map((action) => action.textContent)).toEqual([
      'Join',
      'DM',
      'Create',
      'Find',
    ])
    expect(actions.map((action) => action.getAttribute('href'))).toEqual([
      '/rooms/discover#join',
      '/rooms/dm#dm',
      '/rooms/create#create',
      '/rooms/discover#find',
    ])
  })

  it('opens the room actions menu with +', async () => {
    const { container, findByText, getByRole } = renderPage()

    expect(await findByText('Ops')).toBeTruthy()
    const details = roomActionsDetails(container)
    expect(details.open).toBe(false)

    fireEvent.keyDown(document.body, { key: '+' })

    await waitFor(() => expect(details.open).toBe(true))
    await waitFor(() =>
      expect(document.activeElement).toBe(getByRole('link', { name: 'Join' })),
    )
  })

  it('closes the room actions menu after navigation', async () => {
    const { container, findByText, getByRole } = renderPage()

    expect(await findByText('Ops')).toBeTruthy()
    const details = roomActionsDetails(container)
    details.open = true

    fireEvent.click(getByRole('link', { name: 'Join' }))

    await waitFor(() => expect(details.open).toBe(false))
  })

  it('closes the room actions menu after hash-only navigation', async () => {
    history.replaceState(null, '', '/rooms/discover#join')
    const { container, findByText, getByRole } = renderPage()

    expect(await findByText('Ops')).toBeTruthy()
    const details = roomActionsDetails(container)
    details.open = true

    fireEvent.click(getByRole('link', { name: 'Find' }))

    await waitFor(() => expect(details.open).toBe(false))
  })

  it('closes the room actions menu with Escape', async () => {
    const { container, findByText, getByRole } = renderPage()

    expect(await findByText('Ops')).toBeTruthy()
    const details = roomActionsDetails(container)
    details.open = true

    fireEvent.keyDown(getByRole('link', { name: 'Join' }), {
      key: 'Escape',
    })

    expect(details.open).toBe(false)
  })

  it('closes the room actions menu after clicking outside', async () => {
    const { container, findByText, getByRole } = renderPage()

    expect(await findByText('Ops')).toBeTruthy()
    const details = roomActionsDetails(container)
    details.open = true

    fireEvent.pointerDown(getByRole('link', { name: 'Join' }))
    expect(details.open).toBe(true)

    fireEvent.pointerDown(document.body)

    await waitFor(() => expect(details.open).toBe(false))
  })

  it('opens a room on a mobile tap', async () => {
    history.replaceState(null, '', '/')
    const { findByText } = renderPage()
    const ops = await findByText('Ops')
    const link = ops.closest('a.room-link')!

    fireEvent(
      link,
      roomPointerEvent('pointerdown', {
        pointerId: 1,
        pointerType: 'touch',
        clientX: 20,
        clientY: 20,
      }),
    )
    fireEvent(
      link,
      roomPointerEvent('pointerup', {
        pointerId: 1,
        pointerType: 'touch',
        clientX: 20,
        clientY: 20,
      }),
    )

    expect(location.pathname).toBe(
      `/${ACCOUNT}/rooms/${encodeURIComponent('!ops:hs')}`,
    )
  })

  it('opens a room click with one history entry', async () => {
    history.replaceState(null, '', '/')
    const { findByText } = renderPage()
    const ops = await findByText('Ops')
    const link = ops.closest('a.room-link')!
    const pushState = vi.spyOn(history, 'pushState')

    const click = new MouseEvent('click', { bubbles: true, cancelable: true })
    link.dispatchEvent(click)

    await waitFor(() =>
      expect(location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent('!ops:hs')}`,
      ),
    )
    expect(click.defaultPrevented).toBe(true)
    expect(pushState).toHaveBeenCalledTimes(1)
    pushState.mockRestore()

    history.back()
    await waitFor(() => expect(location.pathname).toBe('/'))
  })

  it('does not open a room when a mobile touch scrolls the list', async () => {
    history.replaceState(null, '', '/')
    const { findByText } = renderPage()
    const ops = await findByText('Ops')
    const link = ops.closest('a.room-link')!

    fireEvent(
      link,
      roomPointerEvent('pointerdown', {
        pointerId: 1,
        pointerType: 'touch',
        clientX: 20,
        clientY: 20,
      }),
    )
    fireEvent(
      link,
      roomPointerEvent('pointermove', {
        pointerId: 1,
        pointerType: 'touch',
        clientX: 20,
        clientY: 60,
      }),
    )
    fireEvent(
      link,
      roomPointerEvent('pointerup', {
        pointerId: 1,
        pointerType: 'touch',
        clientX: 20,
        clientY: 60,
      }),
    )
    expect(location.pathname).toBe('/')
    fireEvent.click(link)

    expect(location.pathname).toBe('/')
  })

  it('renders room avatars through the media proxy', async () => {
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/media/${ACCOUNT}/hs/avatar`,
        () =>
          new HttpResponse('avatar-bytes', {
            headers: { 'content-type': 'image/png' },
          }),
      ),
    )
    const { findByText, container } = renderPage([
      { ...OPS, avatar_url: 'mxc://hs/avatar' },
    ])

    await findByText('Ops')
    await waitFor(() => {
      const avatar = container.querySelector<HTMLImageElement>(
        '.room-row .room-avatar img',
      )
      expect(avatar?.src).toMatch(/^blob:/)
    })
    const link = container.querySelector('.room-link')!
    expect(link.querySelector('.room-avatar')).toBeTruthy()
    expect(link.querySelector('.room-copy .room-name')?.textContent).toBe('Ops')
  })

  it('renders colored fallback initials when a room has no avatar', async () => {
    const { findByText, container } = renderPage([OPS])

    await findByText('Ops')
    const avatar = container.querySelector<HTMLElement>('.room-avatar')!
    expect(avatar.textContent).toBe('O')
    expect(avatar.className).toMatch(/\broom-avatar-color-\d\b/)
    expect(avatar.querySelector('img')).toBeNull()
  })

  it('filters: DMs, groups, favorites, and name query', async () => {
    const { services, findByText, getByRole, getByLabelText, queryByText } =
      renderPage()
    await findByText('Ops')

    fireEvent.click(getByRole('button', { name: 'DMs' }))
    expect(queryByText('Ops')).toBeNull()
    expect(await findByText('Bob')).toBeTruthy()

    fireEvent.click(getByRole('button', { name: 'Groups' }))
    expect(await findByText('Ops')).toBeTruthy()
    expect(queryByText('Bob')).toBeNull()

    services.settings.pinRoom(roomKey(LOUNGE as never))
    fireEvent.click(getByRole('button', { name: 'Favorites' }))
    expect(await findByText('#lounge:hs')).toBeTruthy()
    expect(queryByText('Ops')).toBeNull()

    // Name query overrides the category and matches the rendered title.
    fireEvent.input(getByLabelText('Filter by name'), {
      target: { value: 'bob' },
    })
    expect(await findByText('Bob')).toBeTruthy()
    expect(queryByText('#lounge:hs')).toBeNull()
  })

  it('unread filter uses server-derived notification counts', async () => {
    const unreadOps = { ...OPS, notification_count: 1 }
    const { findByText, getByRole, queryByText } = renderPage([unreadOps, DM])
    await findByText('Ops')

    fireEvent.click(getByRole('button', { name: 'Unread' }))
    expect(await findByText('Ops')).toBeTruthy()
    expect(await findByText('1')).toBeTruthy() // badge
    expect(queryByText('Bob')).toBeNull()
  })

  it('does not treat recent rooms with zero server counts as unread', async () => {
    const freshOps = {
      ...OPS,
      last_activity_ts: 200,
      last_event_id: '$ops-new',
    }
    const { findByText, getByRole, queryByText } = renderPage([freshOps])
    await findByText('Ops')

    fireEvent.click(getByRole('button', { name: 'Unread' }))

    expect(queryByText('Ops')).toBeNull()
    expect(await findByText('No rooms match the current filter.')).toBeTruthy()
  })

  it('unread filter includes rooms with nonzero server counts', async () => {
    const freshOps = {
      ...OPS,
      last_activity_ts: 200,
      last_event_id: '$ops-new',
      notification_count: 3,
    }
    const { findByText, getByRole } = renderPage([freshOps])
    await findByText('Ops')

    fireEvent.click(getByRole('button', { name: 'Unread' }))

    expect(await findByText('Ops')).toBeTruthy()
    expect(await findByText('3')).toBeTruthy()
  })

  it('pinning floats a room to the top with a separator, persisted', async () => {
    const { services, findByText, container } = renderPage()
    await findByText('Ops')

    const titles = () =>
      [...container.querySelectorAll('.room-title')].map(
        (el) => el.childNodes[0]?.textContent,
      )
    // Recent-activity default: DM & Work (now) before Ops before lounge.
    expect(titles().at(-1)).toBe('#lounge:hs')

    // Pin the oldest room; it floats to the top and a separator appears.
    const lounge = [...container.querySelectorAll('.room-row')].find((row) =>
      row.textContent!.includes('#lounge:hs'),
    )!
    fireEvent.click(lounge.querySelector('button.pin')!)

    await waitFor(() => expect(titles()[0]).toBe('#lounge:hs'))
    expect(container.querySelector('.room-separator')).toBeTruthy()
    expect(services.settings.pinnedRooms.value).toEqual([
      roomKey(LOUNGE as never),
    ])
  })

  it('sort modes reorder the unpinned tail', async () => {
    const { findByText, getByLabelText, container } = renderPage([
      OPS,
      LOUNGE,
      OTHER,
    ])
    await findByText('Ops')

    fireEvent.change(getByLabelText('Sort'), { target: { value: 'az' } })
    const titles = [...container.querySelectorAll('.room-title')].map(
      (el) => el.childNodes[0]?.textContent,
    )
    expect(titles).toEqual(['#lounge:hs', 'Ops', 'Work'])
  })

  it('account dropdown narrows to one account', async () => {
    const { findByText, getByLabelText, queryByText } = renderPage()
    await findByText('Work')

    await waitFor(() => expect(getByLabelText('Account')).toBeTruthy())
    fireEvent.change(getByLabelText('Account'), {
      target: { value: OTHER_ACCOUNT },
    })
    expect(await findByText('Work')).toBeTruthy()
    expect(queryByText('Ops')).toBeNull()
  })
})

describe('RoomList keyboard shortcuts (ADR 0078)', () => {
  const ctrl = { ctrlKey: true }

  it('Ctrl-K focuses the name filter and un-collapses the sidebar', async () => {
    const { services, findByText, getByLabelText } = renderPage()
    await findByText('Ops')
    services.settings.sidebarCollapsed.value = true

    fireEvent.keyDown(document.body, { key: 'k', ...ctrl })

    await waitFor(() =>
      expect(document.activeElement).toBe(getByLabelText('Filter by name')),
    )
    expect(services.settings.sidebarCollapsed.value).toBe(false)
  })

  it('Ctrl-K reaches the filter even from inside the composer', async () => {
    const { findByText, getByLabelText } = renderPage()
    await findByText('Ops')
    // A modifier chord fires while typing; a bare one must not.
    const textarea = document.createElement('textarea')
    document.body.append(textarea)
    textarea.focus()

    fireEvent.keyDown(textarea, { key: 'k', ...ctrl })

    await waitFor(() =>
      expect(document.activeElement).toBe(getByLabelText('Filter by name')),
    )
    textarea.remove()
  })

  it('Ctrl-Shift-Y cycles the filter in the TUI’s order and drops the name query', async () => {
    const { services, findByText, getByLabelText } = renderPage()
    await findByText('Ops')
    fireEvent.input(getByLabelText('Filter by name'), {
      target: { value: 'ops' },
    })

    const cycle = ['dms', 'groups', 'unread', 'favorites', 'all']
    for (const expected of cycle) {
      fireEvent.keyDown(document.body, {
        key: 'Y',
        ctrlKey: true,
        shiftKey: true,
      })
      expect(services.settings.roomFilter.value).toBe(expected)
    }
    // Cycling a category clears the name filter, as in the TUI (ADR 0042).
    expect((getByLabelText('Filter by name') as HTMLInputElement).value).toBe(
      '',
    )
  })

  it('Ctrl-Shift-S cycles the sort in the TUI’s order', async () => {
    const { services, findByText } = renderPage()
    await findByText('Ops')

    for (const expected of ['oldest', 'az', 'za', 'recent']) {
      fireEvent.keyDown(document.body, {
        key: 'S',
        ctrlKey: true,
        shiftKey: true,
      })
      expect(services.settings.roomSort.value).toBe(expected)
    }
  })

  it('Ctrl-Up and Ctrl-Down step rooms on Windows and Linux', async () => {
    history.replaceState(null, '', '/')
    const { findByText } = renderPage([OPS, LOUNGE])
    await findByText('Ops')

    fireEvent.keyDown(document.body, { key: 'ArrowDown', ctrlKey: true })

    await waitFor(() =>
      expect(location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent('!ops:hs')}`,
      ),
    )
  })

  it('uses Cmd-Option-Up and Cmd-Option-Down for room stepping on macOS', async () => {
    vi.stubGlobal('navigator', {
      ...navigator,
      userAgent: 'MacIntel',
      maxTouchPoints: 0,
    })
    history.replaceState(null, '', '/')
    const { findByText } = renderPage([OPS, LOUNGE])
    await findByText('Ops')

    fireEvent.keyDown(document.body, { key: 'ArrowDown', metaKey: true })
    expect(location.pathname).toBe('/')

    fireEvent.keyDown(document.body, {
      key: 'ArrowDown',
      metaKey: true,
      altKey: true,
    })

    await waitFor(() =>
      expect(location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent('!ops:hs')}`,
      ),
    )
  })

  it('arrows rove focus across the room links and back to the filter', async () => {
    const { findByText, getByLabelText, container } = renderPage()
    await findByText('Ops')
    const filter = getByLabelText('Filter by name')
    const links = [
      ...container.querySelectorAll<HTMLAnchorElement>('a.room-link'),
    ]
    filter.focus()

    fireEvent.keyDown(filter, { key: 'ArrowDown' })
    expect(document.activeElement).toBe(links[0])

    fireEvent.keyDown(links[0], { key: 'ArrowDown' })
    expect(document.activeElement).toBe(links[1])

    fireEvent.keyDown(links[1], { key: 'ArrowUp' })
    expect(document.activeElement).toBe(links[0])

    // Up off the top returns to where the sequence began.
    fireEvent.keyDown(links[0], { key: 'ArrowUp' })
    expect(document.activeElement).toBe(filter)
  })

  it('clears the room filter with the clear button and keeps focus in the filter', async () => {
    const { findByText, getByLabelText, getByRole, queryByRole } = renderPage()
    await findByText('Ops')
    const filter = getByLabelText('Filter by name') as HTMLInputElement

    fireEvent.input(filter, { target: { value: 'ops' } })
    expect(getByRole('button', { name: 'Clear room filter' })).toBeTruthy()

    fireEvent.click(getByRole('button', { name: 'Clear room filter' }))

    expect(filter.value).toBe('')
    expect(queryByRole('button', { name: 'Clear room filter' })).toBeNull()
    expect(document.activeElement).toBe(filter)
  })

  it('Escape clears a non-empty room filter before asking for the composer', async () => {
    const { services, findByText, getByLabelText } = renderPage()
    await findByText('Ops')
    const filter = getByLabelText('Filter by name') as HTMLInputElement
    fireEvent.input(filter, { target: { value: 'ops' } })
    const before = services.composerFocus.value

    fireEvent.keyDown(filter, { key: 'Escape' })

    expect(filter.value).toBe('')
    expect(services.composerFocus.value).toBe(before)

    fireEvent.keyDown(filter, { key: 'Escape' })

    expect(services.composerFocus.value).toBe(before + 1)
  })

  it('Escape from an empty room list asks for the composer', async () => {
    const { services, findByText, getByLabelText } = renderPage()
    await findByText('Ops')
    const before = services.composerFocus.value

    fireEvent.keyDown(getByLabelText('Filter by name'), { key: 'Escape' })

    expect(services.composerFocus.value).toBe(before + 1)
  })

  it('Enter on a focused room row asks the new room composer for focus', async () => {
    const { services, findByText } = renderPage()
    const rowText = await findByText('Ops')
    const row = rowText.closest<HTMLAnchorElement>('a.room-link')!
    const before = services.composerFocus.value

    row.focus()
    fireEvent.keyDown(row, { key: 'Enter' })

    await waitFor(() => expect(services.composerFocus.value).toBe(before + 1))
    expect(window.location.pathname).toBe(
      `/${ACCOUNT}/rooms/${encodeURIComponent('!ops:hs')}`,
    )
  })

  it('Enter in the name filter opens the only matching room and asks for composer focus', async () => {
    const { services, findByText, getByLabelText } = renderPage()
    await findByText('Ops')
    const filter = getByLabelText('Filter by name') as HTMLInputElement
    const before = services.composerFocus.value

    fireEvent.input(filter, { target: { value: 'lou' } })
    expect(await findByText('#lounge:hs')).toBeTruthy()
    fireEvent.keyDown(filter, { key: 'Enter' })

    await waitFor(() => expect(services.composerFocus.value).toBe(before + 1))
    expect(window.location.pathname).toBe(
      `/${ACCOUNT}/rooms/${encodeURIComponent('!lounge:hs')}`,
    )
  })
})

describe('RoomList account labels', () => {
  it('rows carry the localpart while the dropdown keeps full ids', async () => {
    const { findByText, container, getByLabelText } = renderPage()
    await findByText('Ops')

    const metas = [
      ...container.querySelectorAll('.room-meta, .room-title-meta'),
    ].map((el) => el.textContent ?? '')
    expect(metas.some((m) => m.startsWith('@me ·'))).toBe(true)
    expect(metas.some((m) => m.startsWith('@work ·'))).toBe(true)
    expect(metas.some((m) => m.includes('example.org'))).toBe(false)

    const options = [
      ...getByLabelText('Account').querySelectorAll('option'),
    ].map((el) => el.textContent)
    expect(options).toContain('@me:example.org')
    expect(options).toContain('@work:example.org')
  })

  it('omits the account entirely when only one account has rooms', async () => {
    const { findByText, container } = renderPage([OPS, LOUNGE])
    await findByText('Ops')

    const metas = [
      ...container.querySelectorAll('.room-meta, .room-title-meta'),
    ].map((el) => el.textContent ?? '')
    expect(metas.every((m) => !m.includes('@me'))).toBe(true)
  })

  it('omits the account selector when stored rooms belong to one active account', async () => {
    const { findByText, queryByLabelText, queryByText } = renderPage(
      [OPS, OTHER],
      undefined,
      { activeAccounts: [makeAccount(ACCOUNT, '@me:example.org')] },
    )
    await findByText('Ops')

    await waitFor(() => {
      expect(queryByLabelText('Account')).toBeNull()
      expect(queryByText('Work')).toBeNull()
    })
  })

  it('hides rooms and account filters for logged-out accounts', async () => {
    const { findByText, queryByLabelText, queryByText } = renderPage(
      [OPS, OTHER],
      undefined,
      {
        activeAccounts: [
          makeAccount(ACCOUNT, '@me:example.org'),
          {
            ...makeAccount(OTHER_ACCOUNT, '@work:example.org'),
            state: 'deactivated',
          },
        ],
      },
    )
    await findByText('Ops')

    await waitFor(() => {
      expect(queryByLabelText('Account')).toBeNull()
      expect(queryByText('Work')).toBeNull()
    })
  })

  it('gives the room name its own ellipsizable box', async () => {
    const { findByText, container } = renderPage()
    await findByText('Ops')

    const names = [...container.querySelectorAll('.room-title .room-name')]
    expect(names.length).toBe(container.querySelectorAll('.room-row').length)
    expect(names.map((el) => el.textContent)).toContain('Ops')
  })

  it('shows stripped latest-message previews by default', async () => {
    const previewRoom = {
      ...OPS,
      last_event_id: '$latest',
      last_activity_ts: Date.now() - 60_000,
    }
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                {
                  account_id: ACCOUNT,
                  room_id: '!ops:hs',
                  event_id: '$latest',
                  sender: '@bob:example.org',
                  origin_ts: previewRoom.last_activity_ts,
                  type: 'm.room.message',
                  body: '**this** is the [start](https://example.org) of the message',
                  content: {
                    msgtype: 'm.text',
                    body: '**this** is the [start](https://example.org) of the message',
                  },
                  redacted: false,
                  edited: false,
                  edit_count: 0,
                },
              ],
              next_cursor: null,
            },
          }),
      ),
    )
    const { findByText, container } = renderPage([previewRoom])

    expect(
      await findByText('Bob: this is the start of the message'),
    ).toBeTruthy()
    expect(container.querySelector('.room-title-meta')?.textContent).toBe('1m')
    expect(container.querySelector('.room-meta')).toBeNull()
  })

  it('previews the latest text message when newer events are non-text', async () => {
    const previewRoom = {
      ...OPS,
      last_event_id: '$reaction',
      last_activity_ts: Date.now() - 60_000,
    }
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                {
                  account_id: ACCOUNT,
                  room_id: '!ops:hs',
                  event_id: '$reaction',
                  sender: '@me:example.org',
                  origin_ts: previewRoom.last_activity_ts,
                  type: 'm.reaction',
                  body: null,
                  content: {},
                  redacted: false,
                  edited: false,
                  edit_count: 0,
                  relates_to: {
                    rel_type: 'm.annotation',
                    event_id: '$text',
                    key: '👍',
                  },
                },
                {
                  account_id: ACCOUNT,
                  room_id: '!ops:hs',
                  event_id: '$state',
                  sender: '@me:example.org',
                  origin_ts: previewRoom.last_activity_ts - 1,
                  type: 'm.room.topic',
                  state_key: '',
                  body: 'topic',
                  content: { topic: 'topic' },
                  redacted: false,
                  edited: false,
                  edit_count: 0,
                },
                {
                  account_id: ACCOUNT,
                  room_id: '!ops:hs',
                  event_id: '$text',
                  sender: '@bob:example.org',
                  origin_ts: previewRoom.last_activity_ts - 2,
                  type: 'm.room.message',
                  body: 'the previous text message',
                  content: {
                    msgtype: 'm.text',
                    body: 'the previous text message',
                  },
                  redacted: false,
                  edited: false,
                  edit_count: 0,
                },
              ],
              next_cursor: null,
            },
          }),
      ),
    )
    const { findByText } = renderPage([previewRoom], (services) => {
      services.settings.previewRoom.value = true
    })

    expect(await findByText('Bob: the previous text message')).toBeTruthy()
  })

  it('skips a redacted latest message and previews the one before it', async () => {
    const previewRoom = {
      ...OPS,
      last_event_id: '$redacted',
      last_activity_ts: Date.now() - 60_000,
    }
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/timeline`,
        () =>
          HttpResponse.json({
            data: {
              events: [
                // A removed message keeps its `m.room.message` type but loses
                // its body: previewable-looking, unpreviewable in fact.
                {
                  account_id: ACCOUNT,
                  room_id: '!ops:hs',
                  event_id: '$redacted',
                  sender: '@bob:example.org',
                  origin_ts: previewRoom.last_activity_ts,
                  type: 'm.room.message',
                  body: null,
                  content: {},
                  redacted: true,
                  edited: false,
                  edit_count: 0,
                },
                {
                  account_id: ACCOUNT,
                  room_id: '!ops:hs',
                  event_id: '$text',
                  sender: '@bob:example.org',
                  origin_ts: previewRoom.last_activity_ts - 1,
                  type: 'm.room.message',
                  body: 'still here',
                  content: { msgtype: 'm.text', body: 'still here' },
                  redacted: false,
                  edited: false,
                  edit_count: 0,
                },
              ],
              next_cursor: null,
            },
          }),
      ),
    )
    const { findByText } = renderPage([previewRoom], (services) => {
      services.settings.previewRoom.value = true
    })

    expect(await findByText('Bob: still here')).toBeTruthy()
  })
})

describe('RoomList selection and shortcut hints', () => {
  it('marks the open room with aria-current, and only that one', async () => {
    const { services, findByText, container } = renderPage()
    await findByText('Ops')

    expect(container.querySelectorAll('[aria-current]').length).toBe(0)

    services.activeRoom.value = roomKey(OPS as never)

    await waitFor(() => {
      const current = container.querySelectorAll('a.room-link[aria-current]')
      expect(current.length).toBe(1)
      expect(current[0].getAttribute('aria-current')).toBe('page')
      expect(current[0].textContent).toContain('Ops')
    })

    // Switching rooms moves the marker rather than adding a second one.
    services.activeRoom.value = roomKey(LOUNGE as never)
    await waitFor(() => {
      const current = container.querySelectorAll('a.room-link[aria-current]')
      expect(current.length).toBe(1)
      expect(current[0].textContent).toContain('#lounge:hs')
    })
  })

  it('the room controls advertise the chords that drive them', async () => {
    const { findByText, getByLabelText, container } = renderPage()
    await findByText('Ops')

    const nameFilter = getByLabelText('Filter by name')
    expect(nameFilter.getAttribute('title')).toBe(
      'Filter rooms by name (Ctrl-K)',
    )
    expect(nameFilter.getAttribute('aria-keyshortcuts')).toBe('Control+K')

    const sort = getByLabelText('Sort')
    expect(sort.getAttribute('title')).toBe('Cycle sort order (Ctrl-Shift-S)')
    expect(sort.getAttribute('aria-keyshortcuts')).toBe('Control+Shift+S')

    // The chord cycles the group, so it is announced there, not per chip.
    const group = container.querySelector('.filter-group')!
    expect(group.getAttribute('aria-keyshortcuts')).toBe('Control+Shift+Y')
    const dms = [...group.querySelectorAll('button')].find(
      (b) => b.textContent === 'DM',
    )!
    expect(dms.getAttribute('aria-label')).toBe('DMs')
    expect(dms.getAttribute('title')).toBe(
      'Show DMs — cycle filters (Ctrl-Shift-Y)',
    )
    expect(dms.getAttribute('aria-keyshortcuts')).toBeNull()
  })

  /**
   * A count update in one room must wake one row. The room list used to read
   * unread state from one map-valued signal, so every room subscribed to every
   * update. At 1594 rooms that was 1594 renders and ~0.9MB of VDOM garbage per
   * event — the room list's whole performance problem.
   *
   * jsdom has no layout, so the list here renders unwindowed, which is exactly
   * what makes the fan-out observable: every row is mounted and able to render.
   */
  it('an event in one room re-renders only that room’s row', async () => {
    const renders = new Map<string, number>()
    const previous = options.diffed
    options.diffed = (vnode) => {
      if (typeof vnode.type === 'function') {
        const name = vnode.type.name
        renders.set(name, (renders.get(name) ?? 0) + 1)
      }
      previous?.(vnode)
    }
    try {
      const { services, findByText } = renderPage()
      await findByText('Ops')
      await waitFor(() => expect(renders.get('RoomRow')).toBeGreaterThan(0))

      renders.clear()
      services.rooms.noteUnreadCounts(ACCOUNT, '!ops:hs', 2, 0)
      await waitFor(() => expect(renders.get('RoomRow')).toBe(1))

      // And the badge really did land, so the single render is the right one.
      expect(await findByText('2')).toBeTruthy()
    } finally {
      options.diffed = previous
    }
  })
})
