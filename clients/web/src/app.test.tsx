import {
  cleanup,
  fireEvent,
  render,
  waitFor,
  within,
} from '@testing-library/preact'
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
import { App, accountIdForRoomEntry } from './app.tsx'
import { layoutMode, SINGLE_PANE_QUERY } from './layout'
import type { Account } from './stores/accounts'
import type { EventDto } from './stores/timeline'
import { memoryStorage } from './test/memory-storage'
import { TEST_BASE_URL, testServices } from './test/services'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const ORIGINAL_STANDALONE = (navigator as Navigator & { standalone?: boolean })
  .standalone
const ORIGINAL_INNER_HEIGHT = window.innerHeight
const ACCOUNT_DTO: Account = {
  account_id: ACCOUNT,
  user_id: '@alice:example.org',
  homeserver_url: 'https://matrix.example.org',
  state: 'active',
  sync_state: 'ready',
  verified: true,
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
}

const server = setupServer(
  http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
    HttpResponse.json({ data: [ACCOUNT_DTO] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/rooms`, () => HttpResponse.json({ data: [] })),
  http.get(`${TEST_BASE_URL}/v1/status`, () =>
    HttpResponse.json({
      data: { backfill: { paused: false, free_bytes: 0, accounts: [] } },
    }),
  ),
  // Outbound read receipts + typing notices (ADR 0067/0068) fire from the room
  // read/compose choke points; accepted as no-ops by default.
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
  vi.restoreAllMocks()
  server.resetHandlers()
  Object.defineProperty(window, 'visualViewport', {
    configurable: true,
    value: undefined,
  })
  Object.defineProperty(window, 'innerHeight', {
    configurable: true,
    value: ORIGINAL_INNER_HEIGHT,
  })
  document.documentElement.removeAttribute('style')
  Object.defineProperty(navigator, 'standalone', {
    configurable: true,
    value: ORIGINAL_STANDALONE,
  })
  history.replaceState(null, '', '/')
})
afterAll(() => server.close())

describe('App', () => {
  it('gates on the auth provider: signed out renders the login bootstrap', () => {
    const services = testServices({ token: null })
    const { getByRole } = render(<App services={services} />)
    expect(getByRole('button', { name: 'Sign in' })).toBeTruthy()
  })

  it('signed in renders the shell with settings, brand home, and the room-list sidebar', async () => {
    const services = testServices()
    const { getByRole, queryByRole } = render(<App services={services} />)

    expect(queryByRole('button', { name: 'Sign in' })).toBeNull()
    expect(queryByRole('link', { name: 'Accounts' })).toBeNull()
    expect(queryByRole('link', { name: 'Rooms' })).toBeNull()
    expect(getByRole('link', { name: 'axon' }).getAttribute('href')).toBe('/')
    expect(getByRole('link', { name: 'Settings' })).toBeTruthy()
    await waitFor(() =>
      expect(getByRole('navigation', { name: 'Rooms' })).toBeTruthy(),
    )
  })

  it('groups connection with the brand and keeps utility actions together', () => {
    const { getByRole } = render(<App services={testServices()} />)
    const brand = getByRole('link', { name: 'axon' })
    const connection = getByRole('status')
    const threads = getByRole('button', { name: 'Unread threads' })
    const search = getByRole('button', { name: 'Search messages' })
    const settings = getByRole('link', { name: 'Settings' })
    const help = getByRole('button', { name: 'Keyboard shortcuts' })

    expect(brand.parentElement).toBe(connection.parentElement)
    expect(threads.parentElement).toBe(search.parentElement)
    expect(search.parentElement).toBe(settings.parentElement)
    expect(settings.parentElement).toBe(help.parentElement)
    expect(threads.getAttribute('title')).toBe(
      'Unread threads (/unreadthreads)',
    )
    expect(search.getAttribute('title')).toBe(
      'Search messages (/search; / or Ctrl-Shift-F)',
    )
    expect(threads.compareDocumentPosition(search)).toBe(
      Node.DOCUMENT_POSITION_FOLLOWING,
    )
    expect(search.compareDocumentPosition(settings)).toBe(
      Node.DOCUMENT_POSITION_FOLLOWING,
    )
    expect(settings.compareDocumentPosition(help)).toBe(
      Node.DOCUMENT_POSITION_FOLLOWING,
    )
  })

  it('intercepts Matrix.to room links and joins them in the active account', async () => {
    installRoomPageHandlers()
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
              account_user_id: ACCOUNT_DTO.user_id,
              room_id: joinedRoom,
              name: 'Joined',
              canonical_alias: '#joined:hs',
              topic: null,
              last_activity_ts: 0,
            },
          ],
        }),
      ),
    )
    const services = testServices()
    render(<App services={services} />)
    await waitFor(() =>
      expect(services.accounts.accounts.value).toHaveLength(1),
    )
    const anchor = document.createElement('a')
    anchor.href = 'https://matrix.to/#/%23joined%3Ahs?via=hs'
    anchor.target = '_blank'
    anchor.textContent = 'Join'
    document.body.append(anchor)

    fireEvent.click(anchor)

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(joinedRoom)}`,
      ),
    )
    expect(joinBody).toEqual({
      room_id_or_alias: '#joined:hs',
      server_names: ['hs'],
    })
    anchor.remove()
  })

  it('intercepts matrix:room links and joins them in the active account', async () => {
    installRoomPageHandlers()
    const joinedRoom = '!joined:bostoncoop.net'
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
    const services = testServices()
    render(<App services={services} />)
    await waitFor(() =>
      expect(services.accounts.accounts.value).toHaveLength(1),
    )
    const anchor = document.createElement('a')
    anchor.href = 'matrix:room/a:bostoncoop.net'
    anchor.target = '_blank'
    anchor.textContent = 'Join'
    document.body.append(anchor)

    fireEvent.click(anchor)

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(joinedRoom)}`,
      ),
    )
    expect(joinBody).toEqual({
      room_id_or_alias: '#a:bostoncoop.net',
      server_names: [],
    })
    anchor.remove()
  })

  it('surfaces clicked Matrix room-link join failures in the shell', async () => {
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
    const services = testServices()
    const { container, findByText } = render(<App services={services} />)
    await waitFor(() =>
      expect(services.accounts.accounts.value).toHaveLength(1),
    )
    const anchor = document.createElement('a')
    anchor.href = 'matrix:room/private:hs'
    anchor.textContent = 'Join'
    document.body.append(anchor)

    fireEvent.click(anchor)

    expect(
      await findByText('Could not join #private:hs: not invited'),
    ).toBeTruthy()
    expect(container.querySelector('.shell-banner')).toBeTruthy()
    expect(window.location.pathname).toBe('/')
    anchor.remove()
  })

  it('does not throw when resolving room-entry account from a malformed route segment', () => {
    expect(() =>
      accountIdForRoomEntry('/%E0%A4%A/rooms/', [ACCOUNT_DTO], ACCOUNT),
    ).not.toThrow()
    expect(
      accountIdForRoomEntry('/%E0%A4%A/rooms/', [ACCOUNT_DTO], ACCOUNT),
    ).toBeNull()
  })

  it('asks before joining a matrix protocol-handler callback room', async () => {
    installRoomPageHandlers()
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
    history.replaceState(
      null,
      '',
      '/?matrix=matrix%3Ar%2Fjoined%253Ahs%3Fvia%3Dhs',
    )

    const { findByRole } = render(<App services={testServices()} />)

    const dialog = await findByRole('dialog', { name: 'Join room?' })
    expect(dialog.textContent).toContain('#joined:hs')
    expect(window.location.search).toBe('')
    expect(joinBody).toBeNull()

    fireEvent.click(within(dialog).getByRole('button', { name: 'Join' }))

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(joinedRoom)}`,
      ),
    )
    expect(joinBody).toEqual({
      room_id_or_alias: '#joined:hs',
      server_names: ['hs'],
    })
    expect(window.location.search).toBe('')
  })

  it('asks before joining a Matrix.to callback room link', async () => {
    installRoomPageHandlers()
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
    history.replaceState(
      null,
      '',
      `/?matrixLink=${encodeURIComponent('https://matrix.to/#/%23joined%3Ahs?via=hs')}`,
    )

    const { findByRole } = render(<App services={testServices()} />)

    const dialog = await findByRole('dialog', { name: 'Join room?' })
    expect(dialog.textContent).toContain('#joined:hs')
    expect(window.location.search).toBe('')
    expect(joinBody).toBeNull()

    fireEvent.click(within(dialog).getByRole('button', { name: 'Join' }))

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(joinedRoom)}`,
      ),
    )
    expect(joinBody).toEqual({
      room_id_or_alias: '#joined:hs',
      server_names: ['hs'],
    })
    expect(window.location.search).toBe('')
  })

  it('opens an already-joined matrix protocol-handler callback room without confirmation', async () => {
    installRoomPageHandlers()
    const joinedRoom = '!joined:hs'
    let joined = false
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: ACCOUNT_DTO.user_id,
              room_id: joinedRoom,
              name: 'Joined',
              canonical_alias: '#joined:hs',
              topic: null,
              last_activity_ts: 0,
            },
          ],
        }),
      ),
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`, () => {
        joined = true
        return HttpResponse.json({ data: { room_id: joinedRoom } })
      }),
    )
    history.replaceState(
      null,
      '',
      '/?matrix=matrix%3Ar%2Fjoined%253Ahs%3Fvia%3Dhs',
    )

    const { queryByRole } = render(<App services={testServices()} />)

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(joinedRoom)}`,
      ),
    )
    expect(queryByRole('dialog', { name: 'Join room?' })).toBeNull()
    expect(joined).toBe(false)
    expect(window.location.search).toBe('')
  })

  it('opens an already-joined Matrix.to callback room without confirmation', async () => {
    installRoomPageHandlers()
    const joinedRoom = '!joined:hs'
    let joined = false
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: ACCOUNT_DTO.user_id,
              room_id: joinedRoom,
              name: 'Joined',
              canonical_alias: '#joined:hs',
              topic: null,
              last_activity_ts: 0,
            },
          ],
        }),
      ),
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`, () => {
        joined = true
        return HttpResponse.json({ data: { room_id: joinedRoom } })
      }),
    )
    history.replaceState(
      null,
      '',
      `/?matrixLink=${encodeURIComponent('https://matrix.to/#/%23joined%3Ahs?via=hs')}`,
    )

    const { queryByRole } = render(<App services={testServices()} />)

    await waitFor(() =>
      expect(window.location.pathname).toBe(
        `/${ACCOUNT}/rooms/${encodeURIComponent(joinedRoom)}`,
      ),
    )
    expect(queryByRole('dialog', { name: 'Join room?' })).toBeNull()
    expect(joined).toBe(false)
    expect(window.location.search).toBe('')
  })

  it('cancels a matrix protocol-handler callback without joining', async () => {
    let joined = false
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/join`, () => {
        joined = true
        return HttpResponse.json({ data: { room_id: '!joined:hs' } })
      }),
    )
    history.replaceState(null, '', '/?matrix=matrix%3Ar%2Fjoined%253Ahs')

    const { findByRole, queryByRole } = render(
      <App services={testServices()} />,
    )

    const dialog = await findByRole('dialog', { name: 'Join room?' })
    fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }))

    await waitFor(() =>
      expect(queryByRole('dialog', { name: 'Join room?' })).toBeNull(),
    )
    expect(joined).toBe(false)
    expect(window.location.pathname).toBe('/')
    expect(window.location.search).toBe('')
  })

  it('opens the unread-thread drawer from the topbar count', async () => {
    const services = testServices()
    services.threadUnread.recordLiveEvent(
      {
        account_id: ACCOUNT,
        room_id: ROOM,
        event_id: '$reply',
        sender: '@alice:example.org',
        origin_ts: 200,
        type: 'm.room.message',
        body: 'thread reply',
        relates_to: { rel_type: 'm.thread', event_id: '$root' },
      } as unknown as EventDto,
      { roomTitle: 'Ops', rootPreview: 'root body' },
    )
    const { findByRole, findByText, getByRole } = render(
      <App services={services} />,
    )

    fireEvent.click(getByRole('button', { name: 'Unread threads, 1' }))
    await findByRole('dialog', { name: 'Unread threads' })
    expect(await findByText('Ops')).toBeTruthy()
    expect(await findByText('root body')).toBeTruthy()
    expect(await findByText('thread reply')).toBeTruthy()
  })

  it('tracks the visual viewport for iOS keyboard accessory panning', async () => {
    const listeners = new Map<string, EventListener>()
    const viewport = {
      offsetTop: 42,
      offsetLeft: 3,
      width: 1024,
      height: 620,
      addEventListener: vi.fn((type: string, listener: EventListener) => {
        listeners.set(type, listener)
      }),
      removeEventListener: vi.fn(),
    } as unknown as VisualViewport
    Object.defineProperty(window, 'visualViewport', {
      configurable: true,
      value: viewport,
    })

    const { unmount } = render(<App services={testServices()} />)

    await waitFor(() =>
      expect(
        document.documentElement.style.getPropertyValue('--app-viewport-top'),
      ).toBe('42px'),
    )
    expect(
      document.documentElement.style.getPropertyValue('--app-viewport-left'),
    ).toBe('3px')
    expect(
      document.documentElement.style.getPropertyValue('--app-viewport-width'),
    ).toBe('1024px')
    expect(
      document.documentElement.style.getPropertyValue('--app-viewport-height'),
    ).toBe('620px')

    Object.assign(viewport, { offsetTop: 84, height: 580 })
    listeners.get('resize')?.(new Event('resize'))

    expect(
      document.documentElement.style.getPropertyValue('--app-viewport-top'),
    ).toBe('84px')
    expect(
      document.documentElement.style.getPropertyValue('--app-viewport-height'),
    ).toBe('580px')

    unmount()
    expect(
      document.documentElement.style.getPropertyValue('--app-viewport-top'),
    ).toBe('')
  })

  it('refills the viewport when the keyboard leaves with the focused element', async () => {
    // The mobile half-height shell: the keyboard shrinks the visual viewport,
    // then a state change (leaving a thread, following a link) unmounts the
    // focused composer. iOS dismisses the keyboard without firing a viewport
    // `resize`, so the shell must not stay pinned to the keyboard-up height.
    const listeners = new Map<string, EventListener>()
    const viewport = {
      offsetTop: 0,
      offsetLeft: 0,
      width: 390,
      height: 800,
      scale: 1,
      addEventListener: vi.fn((type: string, listener: EventListener) => {
        listeners.set(type, listener)
      }),
      removeEventListener: vi.fn(),
    } as unknown as VisualViewport
    Object.defineProperty(window, 'visualViewport', {
      configurable: true,
      value: viewport,
    })
    Object.defineProperty(window, 'innerHeight', {
      configurable: true,
      value: 800,
    })

    const composer = document.createElement('textarea')
    document.body.appendChild(composer)
    render(<App services={testServices()} />)
    await waitFor(() =>
      expect(
        document.documentElement.style.getPropertyValue(
          '--app-viewport-height',
        ),
      ).toBe('800px'),
    )

    // Keyboard up over a focused field: the shrunken height is honoured.
    composer.focus()
    Object.assign(viewport, { height: 380 })
    listeners.get('resize')?.(new Event('resize'))
    expect(
      document.documentElement.style.getPropertyValue('--app-viewport-height'),
    ).toBe('380px')

    // The state change takes the focused field away. No `resize` follows, and
    // the visual viewport still reports the keyboard-up height.
    composer.dispatchEvent(new FocusEvent('focusout', { bubbles: true }))
    composer.remove()

    await waitFor(() =>
      expect(
        document.documentElement.style.getPropertyValue(
          '--app-viewport-height',
        ),
      ).toBe(''),
    )
    // Cleared, so `.shell` falls back to `100dvh` and fills the screen again.
    expect(
      document.documentElement.style.getPropertyValue('--app-viewport-top'),
    ).toBe('')
  })

  it('keeps tracking a pinch-zoomed viewport with nothing focused', async () => {
    // Same shape as the stale case but legitimately short: zoomed in, so the
    // shell must keep following the visible area instead of snapping back.
    const viewport = {
      offsetTop: 30,
      offsetLeft: 12,
      width: 200,
      height: 400,
      scale: 2,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    } as unknown as VisualViewport
    Object.defineProperty(window, 'visualViewport', {
      configurable: true,
      value: viewport,
    })
    Object.defineProperty(window, 'innerHeight', {
      configurable: true,
      value: 800,
    })

    render(<App services={testServices()} />)
    await waitFor(() =>
      expect(
        document.documentElement.style.getPropertyValue(
          '--app-viewport-height',
        ),
      ).toBe('400px'),
    )
    expect(
      document.documentElement.style.getPropertyValue('--app-viewport-top'),
    ).toBe('30px')
  })

  it('reserves the standalone iOS keyboard accessory strip while editing', async () => {
    vi.spyOn(navigator, 'userAgent', 'get').mockReturnValue(
      'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)',
    )
    vi.spyOn(window, 'matchMedia').mockImplementation(
      (query: string) =>
        ({
          media: query,
          matches: query === '(display-mode: standalone)',
          onchange: null,
          addListener: () => {},
          removeListener: () => {},
          addEventListener: () => {},
          removeEventListener: () => {},
          dispatchEvent: () => false,
        }) as MediaQueryList,
    )
    Object.defineProperty(navigator, 'standalone', {
      configurable: true,
      value: true,
    })

    const { unmount } = render(<App services={testServices()} />)
    const textarea = document.createElement('textarea')
    document.body.append(textarea)

    await waitFor(() =>
      expect(
        document.documentElement.style.getPropertyValue(
          '--app-standalone-composer-bottom-padding',
        ),
      ).toBe('4px'),
    )

    textarea.focus()
    await waitFor(() =>
      expect(
        document.documentElement.style.getPropertyValue(
          '--app-keyboard-accessory-inset',
        ),
      ).toBe('4px'),
    )

    textarea.blur()
    await waitFor(() =>
      expect(
        document.documentElement.style.getPropertyValue(
          '--app-keyboard-accessory-inset',
        ),
      ).toBe(''),
    )

    unmount()
    textarea.remove()
    expect(
      document.documentElement.style.getPropertyValue(
        '--app-keyboard-accessory-inset',
      ),
    ).toBe('')
    expect(
      document.documentElement.style.getPropertyValue(
        '--app-standalone-composer-bottom-padding',
      ),
    ).toBe('')
  })

  it('takes a signed-in user with no accounts to the add-account interface', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const services = testServices()
    const { findByText, getByRole } = render(<App services={services} />)

    expect(await findByText('No accounts yet — add one below.')).toBeTruthy()
    expect(getByRole('heading', { name: 'Accounts' })).toBeTruthy()
    expect(getByRole('button', { name: 'Log in' })).toBeTruthy()
    expect(window.location.pathname).toBe('/accounts')
  })

  it('sign out drops back to the login bootstrap', async () => {
    const services = testServices()
    history.replaceState(null, '', '/settings')
    const { findByRole } = render(<App services={services} />)

    const signOut = await findByRole('button', { name: 'Sign out' })
    signOut.click()

    expect(await findByRole('button', { name: 'Sign in' })).toBeTruthy()
    expect(services.auth.signedIn.value).toBe(false)
  })

  it('Escape returns from Settings to the room list', async () => {
    const services = testServices()
    history.replaceState(null, '', '/settings')
    const { findByRole } = render(<App services={services} />)

    expect(await findByRole('heading', { name: 'Settings' })).toBeTruthy()
    fireEvent.keyDown(document.body, { key: 'Escape' })

    await waitFor(() => expect(window.location.pathname).toBe('/'))
  })

  it('completes an OAuth callback and renders the signed-in shell', async () => {
    server.use(
      http.post(`${TEST_BASE_URL}/v1/oauth/token`, () =>
        HttpResponse.json({
          access_token: 'oauth-access',
          token_type: 'Bearer',
          expires_in: 3600,
          refresh_token: 'oauth-refresh',
        }),
      ),
    )
    const pendingStorage = memoryStorage({
      'axon.oauth.pending': JSON.stringify({
        state: 'state-123',
        codeVerifier: 'verifier-123',
        provider: 'google',
        redirectUri: 'http://localhost:3000/oauth/callback',
        createdAt: Date.now(),
      }),
    })
    const services = testServices({ token: null, pendingStorage })
    history.replaceState(
      null,
      '',
      '/oauth/callback?code=code-1&state=state-123',
    )

    const { findByRole } = render(<App services={services} />)

    expect(await findByRole('navigation', { name: 'Rooms' })).toBeTruthy()
    expect(services.auth.signedIn.value).toBe(true)
  })
})

describe('layoutMode (ADR 0062)', () => {
  it.each([
    ['/', 'rooms'],
    ['/acct-1/rooms/!ops:hs', 'room'],
    ['/acct-1/rooms/%21ops%3Ahs', 'room'],
    ['/accounts', 'utility'],
    ['/settings', 'room-entry'],
    ['/rooms/discover', 'room-entry'],
    ['/rooms/dm', 'room-entry'],
    ['/rooms/create', 'room-entry'],
    ['/nonsense', 'utility'],
    // A room URL missing its room id is not a room surface.
    ['/acct-1/rooms/', 'utility'],
    ['/acct-1/rooms', 'utility'],
  ])('%s → %s', (path, mode) => {
    expect(layoutMode(path)).toBe(mode)
  })
})

function installRoomPageHandlers(): void {
  server.use(
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
      HttpResponse.json({ data: { updated_at: '2026-07-23T12:00:00Z' } }),
    ),
  )
}

describe('shell layout (ADR 0062)', () => {
  const shellBody = (container: Element) =>
    container.querySelector('.shell-body')!

  it('tags the body with the route’s layout mode', async () => {
    const { container, unmount } = render(<App services={testServices()} />)
    await waitFor(() =>
      expect(shellBody(container).className).toContain('mode-rooms'),
    )
    unmount()

    history.replaceState(null, '', '/accounts')
    const utility = render(<App services={testServices()} />)
    await waitFor(() =>
      expect(shellBody(utility.container).className).toContain('mode-utility'),
    )
  })

  it('the sidebar stays mounted on a utility page (CSS hides it)', async () => {
    history.replaceState(null, '', '/accounts')
    const { container, getByRole, queryByRole } = render(
      <App services={testServices()} />,
    )
    await waitFor(() =>
      expect(shellBody(container).className).toContain('mode-utility'),
    )
    expect(getByRole('navigation', { name: 'Rooms' })).toBeTruthy()
    // Nothing to collapse there, so the toggle is gone rather than pointing
    // aria-controls at an already-hidden sidebar.
    expect(queryByRole('button', { name: 'Hide rooms' })).toBeNull()
  })

  it.each(['/rooms/discover', '/rooms/dm', '/rooms/create', '/settings'])(
    'keeps the sidebar controls available on %s',
    async (path) => {
      history.replaceState(null, '', path)
      const { container, getByRole } = render(<App services={testServices()} />)

      await waitFor(() =>
        expect(shellBody(container).className).toContain('mode-room-entry'),
      )
      expect(getByRole('navigation', { name: 'Rooms' })).toBeTruthy()
      expect(getByRole('button', { name: 'Hide rooms' })).toBeTruthy()
    },
  )

  it('the collapse toggle flips aria-expanded and persists the flag', async () => {
    const services = testServices()
    const { container, getByRole } = render(<App services={services} />)
    await waitFor(() => expect(shellBody(container)).toBeTruthy())

    const toggle = getByRole('button', { name: 'Hide rooms' })
    expect(toggle.getAttribute('aria-expanded')).toBe('true')
    expect(toggle.getAttribute('aria-controls')).toBe('room-sidebar')
    expect(shellBody(container).className).not.toContain('sidebar-collapsed')

    fireEvent.click(toggle)

    expect(services.settings.sidebarCollapsed.value).toBe(true)
    expect(shellBody(container).className).toContain('sidebar-collapsed')
    expect(
      getByRole('button', { name: 'Show rooms' }).getAttribute('aria-expanded'),
    ).toBe('false')

    fireEvent.click(getByRole('button', { name: 'Show rooms' }))
    expect(services.settings.sidebarCollapsed.value).toBe(false)
  })

  it('auto-hides a lone space until the user opens it', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: ACCOUNT_DTO.user_id,
              room_id: '!only-space:example.org',
              name: 'Only space',
              room_type: 'm.space',
              last_activity_ts: 0,
            },
          ],
        }),
      ),
    )
    const services = testServices()
    const { container, getByRole } = render(<App services={services} />)

    await waitFor(() =>
      expect(getByRole('button', { name: 'Show spaces' })).toBeTruthy(),
    )
    expect(shellBody(container).className).toContain('spaces-pane-collapsed')

    fireEvent.click(getByRole('button', { name: 'Show spaces' }))
    expect(services.settings.spacesPaneAutoHide.value).toBe(false)
    expect(shellBody(container).className).not.toContain(
      'spaces-pane-collapsed',
    )
  })

  it('does not render or toggle the sidebar control in single-pane layout', async () => {
    vi.spyOn(window, 'matchMedia').mockImplementation(
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
    const services = testServices()
    const { container, queryByRole } = render(<App services={services} />)
    await waitFor(() => expect(shellBody(container)).toBeTruthy())

    expect(queryByRole('button', { name: 'Hide rooms' })).toBeNull()
    expect(queryByRole('button', { name: 'Show rooms' })).toBeNull()

    fireEvent.keyDown(document.body, { key: 'b', ctrlKey: true })
    expect(services.settings.sidebarCollapsed.value).toBe(false)
  })
})

describe('shell keyboard shortcuts (ADR 0078)', () => {
  it('Ctrl-B toggles the sidebar, and is inert on a utility page', async () => {
    const services = testServices()
    const { container } = render(<App services={services} />)
    await waitFor(() => expect(container.querySelector('.shell-body')))

    fireEvent.keyDown(document.body, { key: 'b', ctrlKey: true })
    expect(services.settings.sidebarCollapsed.value).toBe(true)
    fireEvent.keyDown(document.body, { key: 'b', ctrlKey: true })
    expect(services.settings.sidebarCollapsed.value).toBe(false)

    cleanup()
    history.replaceState(null, '', '/accounts')
    const utility = testServices()
    render(<App services={utility} />)
    await waitFor(() =>
      expect(document.querySelector('.mode-utility')).toBeTruthy(),
    )
    fireEvent.keyDown(document.body, { key: 'b', ctrlKey: true })
    expect(utility.settings.sidebarCollapsed.value).toBe(false)
  })

  it('Ctrl-Alt-M opens the direct-message form from the composer', async () => {
    const { findByRole } = render(<App services={testServices()} />)
    await findByRole('heading', { name: 'Add a Room' })

    fireEvent.keyDown(document.body, { key: 'm', ctrlKey: true, altKey: true })

    await waitFor(() => expect(window.location.pathname).toBe('/rooms/dm'))
  })

  it('? opens help with separate shortcut and command sections, Escape closes it', async () => {
    const { getByRole, queryByRole, findByRole } = render(
      <App services={testServices()} />,
    )

    fireEvent.keyDown(document.body, { key: '?', shiftKey: true })
    await findByRole('dialog', { name: 'Help' })
    const shortcutSection = getByRole('heading', {
      name: 'Keyboard shortcuts',
    }).closest('section')!
    const commandSection = getByRole('heading', {
      name: 'Commands',
    }).closest('section')!
    expect(shortcutSection.textContent).toContain('Ctrl-K')
    expect(shortcutSection.textContent).toContain('Ctrl-Shift-F')
    expect(shortcutSection.textContent).toContain('Ctrl-Alt-M')
    expect(shortcutSection.textContent).toContain('Ctrl-Shift-Y')
    expect(shortcutSection.textContent).toContain('Cycle filter')
    expect(shortcutSection.textContent).toContain('Shift-Enter')
    expect(shortcutSection.textContent).not.toContain('Send')
    expect(shortcutSection.textContent).not.toContain('/react [emoji]')
    expect(commandSection.textContent).toContain('/react [emoji]')
    expect(commandSection.textContent).toContain('/reply [message]')
    expect(commandSection.textContent).toContain('/room <room>')
    expect(commandSection.textContent).toContain('/dm [user]')
    expect(commandSection.textContent).toContain('/jump [YYYY-MM-DD]')
    expect(commandSection.textContent).toContain('/thread')
    expect(commandSection.textContent).toContain('/help')

    fireEvent.keyDown(document.body, { key: 'Escape' })
    await waitFor(() =>
      expect(queryByRole('dialog', { name: 'Help' })).toBeNull(),
    )

    // Re-open and close with the button.
    fireEvent.keyDown(document.body, { key: '?', shiftKey: true })
    await findByRole('dialog', { name: 'Help' })
    fireEvent.click(getByRole('button', { name: 'Close' }))
    await waitFor(() =>
      expect(queryByRole('dialog', { name: 'Help' })).toBeNull(),
    )
  })

  it('shows commands first and collapses shortcut help in single-pane layout', async () => {
    vi.spyOn(window, 'matchMedia').mockImplementation(
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
    const { getByRole, findByRole } = render(<App services={testServices()} />)

    fireEvent.click(getByRole('button', { name: 'Keyboard shortcuts' }))
    const dialog = await findByRole('dialog', { name: 'Help' })
    const commandSection = within(dialog)
      .getByRole('heading', { name: 'Commands' })
      .closest('section')!
    const shortcutDetails = dialog.querySelector(
      '.shortcut-details',
    ) as HTMLDetailsElement

    expect(shortcutDetails).toBeTruthy()
    expect(shortcutDetails.open).toBe(false)
    expect(shortcutDetails.querySelector('summary')?.textContent).toBe(
      'Keyboard shortcuts',
    )
    expect(commandSection.compareDocumentPosition(shortcutDetails)).toBe(
      Node.DOCUMENT_POSITION_FOLLOWING,
    )
    expect(commandSection.textContent).toContain('/help')
    expect(shortcutDetails.textContent).toContain('Ctrl-K')
  })

  it('renders shortcut help with macOS modifier labels on Apple platforms', async () => {
    vi.spyOn(navigator, 'userAgent', 'get').mockReturnValue(
      'Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)',
    )
    const { findByRole } = render(<App services={testServices()} />)

    fireEvent.keyDown(document.body, { key: '?', shiftKey: true })
    const dialog = await findByRole('dialog', { name: 'Help' })

    expect(dialog.textContent).toContain('⌘-K')
    expect(dialog.textContent).toContain('? or ⌘-/')
    expect(dialog.textContent).toContain('/ or ⌘-G')
    expect(dialog.textContent).not.toContain('TUI')
  })

  it('? does not open the help while typing', async () => {
    const { queryByRole } = render(<App services={testServices()} />)
    const input = document.createElement('input')
    document.body.append(input)
    input.focus()

    fireEvent.keyDown(input, { key: '?', shiftKey: true })

    expect(queryByRole('dialog', { name: 'Help' })).toBeNull()
    input.remove()
  })

  it('Ctrl-/ opens the help from a text field, where ? cannot', async () => {
    const { findByRole } = render(<App services={testServices()} />)
    const textarea = document.createElement('textarea')
    document.body.append(textarea)
    textarea.focus()

    // The composer is where focus usually is, and `?` has to reach it as a
    // character — so help needs a chord that survives a text field.
    fireEvent.keyDown(textarea, { key: '?', shiftKey: true })
    fireEvent.keyDown(textarea, { key: '/', ctrlKey: true })

    expect(await findByRole('dialog', { name: 'Help' })).toBeTruthy()
    textarea.remove()
  })

  it('/help opens the shortcut help from the room composer', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: ROOM,
              name: 'Ops',
              last_activity_ts: 0,
            },
          ],
        }),
      ),
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/timeline`,
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
    )
    history.replaceState(
      null,
      '',
      `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
    )
    const { findByLabelText, findByRole } = render(
      <App services={testServices()} />,
    )
    const textarea = (await findByLabelText(
      'Message Ops',
    )) as HTMLTextAreaElement
    expect(
      (await findByRole('button', { name: 'Jump' })).getAttribute('title'),
    ).toBe('Jump to a date (/jump)')

    fireEvent.input(textarea, { target: { value: '/help' } })
    fireEvent.keyDown(textarea, { key: 'Enter' })

    expect(await findByRole('dialog', { name: 'Help' })).toBeTruthy()
    expect(textarea.value).toBe('')
  })

  it('the help button opens the help and names its chords', async () => {
    const { getByRole, findByRole } = render(<App services={testServices()} />)

    const button = getByRole('button', { name: 'Keyboard shortcuts' })
    expect(button.getAttribute('title')).toBe(
      'Keyboard shortcuts (/help; ? or Ctrl-/)',
    )
    expect(button.getAttribute('aria-keyshortcuts')).toBe('? Control+/')

    fireEvent.click(button)
    expect(await findByRole('dialog', { name: 'Help' })).toBeTruthy()
  })

  it('the sidebar toggle advertises its chord', () => {
    const { getByRole } = render(<App services={testServices()} />)
    const toggle = getByRole('button', { name: 'Hide rooms' })
    expect(toggle.getAttribute('title')).toBe(
      'Hide rooms (Ctrl-B); drag or use arrow keys to resize',
    )
    expect(toggle.getAttribute('aria-keyshortcuts')).toBe('Control+B')
  })
})
