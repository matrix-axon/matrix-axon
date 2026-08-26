import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
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
import {
  resetInstallPromptForTest,
  setupInstallPromptCapture,
  type BeforeInstallPromptEvent,
} from '../install-prompt'
import { BUILD_INFO } from '../build-info'
import { TEST_BASE_URL, testServices } from '../test/services'
import { formatServerBuildLine } from './ServerStatus'
import { SettingsPage } from './SettingsPage'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!ops:hs'

let cleanupPromptCapture: (() => void) | null = null

const server = setupServer(
  http.get(`${TEST_BASE_URL}/v1/invites`, () =>
    HttpResponse.json({ data: [] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
    HttpResponse.json({ data: [] }),
  ),
  http.get(`${TEST_BASE_URL}/v1/status`, () =>
    HttpResponse.json({
      data: { backfill: { paused: false, free_bytes: 0, accounts: [] } },
    }),
  ),
)

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  cleanupPromptCapture?.()
  cleanupPromptCapture = null
  resetInstallPromptForTest()
  server.resetHandlers()
  vi.unstubAllGlobals()
})
afterAll(() => server.close())

describe('SettingsPage', () => {
  it('reflects and updates the theme setting', () => {
    const services = testServices()
    const { getByLabelText } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    expect((getByLabelText('System') as HTMLInputElement).checked).toBe(true)

    fireEvent.click(getByLabelText('Dark'))

    expect(services.settings.theme.value).toBe('dark')
    expect((getByLabelText('Dark') as HTMLInputElement).checked).toBe(true)
  })

  it('picks a state-event tier, persisted rather than per-room', () => {
    const services = testServices()
    const { getByLabelText } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )
    const important = getByLabelText(
      'Membership and profile changes',
    ) as HTMLInputElement
    expect(important.checked).toBe(true)

    fireEvent.click(getByLabelText('All state events'))
    expect(services.settings.stateEvents.value).toBe('all')

    fireEvent.click(getByLabelText('Hidden'))
    expect(services.settings.stateEvents.value).toBe('hidden')
  })

  it('toggles hidden deleted messages off by default', () => {
    const services = testServices()
    const { getByLabelText } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )
    const box = getByLabelText('Hide deleted messages') as HTMLInputElement

    expect(box.checked).toBe(false)
    fireEvent.click(box)

    expect(services.settings.hideRedactedEvents.value).toBe(true)
    expect(box.checked).toBe(true)
  })

  it('hides debug settings until Debug is selected', () => {
    const services = testServices()
    const { getByRole, queryByLabelText } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    expect(queryByLabelText('Developer mode')).toBeNull()
    expect(queryByLabelText('Performance instrumentation')).toBeNull()
    expect(queryByLabelText('Correct iOS keyboard page drift')).toBeNull()

    fireEvent.click(getByRole('button', { name: 'Debug' }))

    expect(queryByLabelText('Developer mode')).not.toBeNull()
    expect(queryByLabelText('Performance instrumentation')).not.toBeNull()
    expect(queryByLabelText('Correct iOS keyboard page drift')).not.toBeNull()

    fireEvent.click(getByRole('button', { name: 'Debug' }))

    expect(queryByLabelText('Developer mode')).toBeNull()
  })

  it('toggles developer mode from the Debug section', () => {
    const services = testServices()
    const { getByLabelText, getByRole } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )
    fireEvent.click(getByRole('button', { name: 'Debug' }))
    const box = getByLabelText('Developer mode') as HTMLInputElement

    expect(box.checked).toBe(false)
    fireEvent.click(box)

    expect(services.settings.developerMode.value).toBe(true)
    expect(box.checked).toBe(true)
  })

  it('includes account management and browser sign-out controls', async () => {
    const services = testServices()
    const { findByRole, getByRole } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    expect(getByRole('heading', { name: 'Accounts' })).toBeTruthy()
    expect(await findByRole('button', { name: 'Log in' })).toBeTruthy()

    fireEvent.click(getByRole('button', { name: 'Sign out' }))
    expect(services.auth.signedIn.value).toBe(false)
  })

  it('keeps older server status responses quiet', async () => {
    const services = testServices()
    const { findByRole, queryByRole, queryByText } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    expect(await findByRole('heading', { name: 'Server status' })).toBeTruthy()
    expect(queryByText('Axon server')).toBeNull()
    expect(queryByRole('heading', { name: 'Sync service' })).toBeNull()
  })

  it('shows build and sync details from newer server status responses', async () => {
    const since = Date.UTC(2026, 6, 22, 12, 0, 0)
    server.use(
      http.get(`${TEST_BASE_URL}/v1/status`, () =>
        HttpResponse.json({
          data: {
            backfill: { paused: false, free_bytes: 0, accounts: [] },
            build: {
              version: '0.15.0',
              git_hash: 'abcdef1234567890',
              profile: 'release',
              build_time: '2026-07-22T12:34:56Z',
              rustc_version: 'rustc 1.89.0',
            },
            sync: [
              {
                account_id: ACCOUNT,
                state: 'running',
                since_ms: since,
              },
            ],
          },
        }),
      ),
    )
    const services = testServices()
    const { container, findByRole, findByText, getByText } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    expect(await findByText('0.15.0')).toBeTruthy()
    expect(getByText('abcdef123456')).toBeTruthy()
    expect(container.textContent).toContain('release')
    expect(getByText('rustc 1.89.0')).toBeTruthy()
    expect(await findByRole('heading', { name: 'Sync service' })).toBeTruthy()
    expect(getByText('running')).toBeTruthy()
    expect(getByText(ACCOUNT.slice(0, 8))).toBeTruthy()
    expect(
      container.querySelector(
        `time[datetime="${new Date(since).toISOString()}"]`,
      ),
    ).toBeTruthy()
  })

  it('registers the browser matrix protocol handler from the opt-in checkbox', () => {
    const registerProtocolHandler = vi.fn()
    const restore = mockNavigatorProperty(
      'registerProtocolHandler',
      registerProtocolHandler,
    )
    const services = testServices()
    const { getByLabelText, getByText } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )
    const box = getByLabelText('Handle matrix: links') as HTMLInputElement

    expect(box.checked).toBe(false)
    fireEvent.click(box)

    expect(registerProtocolHandler).toHaveBeenCalledWith(
      'matrix',
      `${window.location.origin}/?matrixLink=%s`,
    )
    expect(services.settings.matrixProtocolHandler.value).toBe(true)
    expect(box.checked).toBe(true)
    expect(
      getByText('Matrix link handling registered for this browser.'),
    ).toBeTruthy()
    restore()
  })

  it('disables matrix protocol registration when unsupported', () => {
    const restore = mockNavigatorProperty('registerProtocolHandler', undefined)
    const services = testServices()
    const { getByLabelText, getByText } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    expect(
      (getByLabelText('Handle matrix: links') as HTMLInputElement).disabled,
    ).toBe(true)
    expect(
      getByText('This browser does not support protocol-handler registration.'),
    ).toBeTruthy()
    restore()
  })

  it('prompts Android users to install when the browser exposes the install event', async () => {
    const restoreUserAgent = mockNavigatorProperty(
      'userAgent',
      'Mozilla/5.0 (Linux; Android 16; Pixel) AppleWebKit/537.36 Chrome/146 Mobile Safari/537.36',
    )
    cleanupPromptCapture = setupInstallPromptCapture(window)
    let prompted = false
    const event = new Event('beforeinstallprompt', {
      cancelable: true,
    }) as BeforeInstallPromptEvent
    Object.assign(event, {
      platforms: ['web'],
      prompt: vi.fn(async () => {
        prompted = true
      }),
      userChoice: Promise.resolve({ outcome: 'accepted', platform: 'web' }),
    })
    window.dispatchEvent(event)
    const services = testServices()
    const { findByRole, findByText } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    fireEvent.click(await findByRole('button', { name: 'Add to home screen' }))

    await waitFor(() => expect(prompted).toBe(true))
    expect(await findByText('Install request accepted.')).toBeTruthy()
    restoreUserAgent()
  })

  it('uses Windows desktop install language when Chrome exposes the install event', async () => {
    const restoreUserAgent = mockNavigatorProperty(
      'userAgent',
      'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/146 Safari/537.36',
    )
    cleanupPromptCapture = setupInstallPromptCapture(window)
    let prompted = false
    const event = new Event('beforeinstallprompt', {
      cancelable: true,
    }) as BeforeInstallPromptEvent
    Object.assign(event, {
      platforms: ['web'],
      prompt: vi.fn(async () => {
        prompted = true
      }),
      userChoice: Promise.resolve({ outcome: 'accepted', platform: 'web' }),
    })
    window.dispatchEvent(event)
    const services = testServices()
    const { findByRole, findByText, getByRole } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    expect(getByRole('heading', { name: 'Desktop app' })).toBeTruthy()
    fireEvent.click(await findByRole('button', { name: 'Add to Start Menu' }))

    await waitFor(() => expect(prompted).toBe(true))
    expect(await findByText('Install request accepted.')).toBeTruthy()
    restoreUserAgent()
  })

  it('shows iOS home-screen instructions when the install prompt is unavailable', () => {
    const restoreUserAgent = mockNavigatorProperty(
      'userAgent',
      'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)',
    )
    const services = testServices()
    const { getByText, queryByRole } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    expect(getByText('Tap the Share button in Safari.')).toBeTruthy()
    expect(getByText('Choose Add to Home Screen.')).toBeTruthy()
    expect(queryByRole('button', { name: 'Add to home screen' })).toBeNull()
    restoreUserAgent()
  })

  it('reveals the Android install button when the prompt arrives after Settings renders', async () => {
    const restoreUserAgent = mockNavigatorProperty(
      'userAgent',
      'Mozilla/5.0 (Linux; Android 16; Pixel) AppleWebKit/537.36 Chrome/146 Mobile Safari/537.36',
    )
    cleanupPromptCapture = setupInstallPromptCapture(window)
    const services = testServices()
    const { findByRole, getByText } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )
    expect(getByText(/Open your browser menu/)).toBeTruthy()
    const event = new Event('beforeinstallprompt', {
      cancelable: true,
    }) as BeforeInstallPromptEvent
    Object.assign(event, {
      platforms: ['web'],
      prompt: vi.fn(async () => {}),
      userChoice: Promise.resolve({ outcome: 'dismissed', platform: 'web' }),
    })

    window.dispatchEvent(event)

    expect(
      await findByRole('button', { name: 'Add to home screen' }),
    ).toBeTruthy()
    restoreUserAgent()
  })

  it('marks all room summaries read after the device-state write lands', async () => {
    let putRequested = false
    let resolvePut!: () => void
    const putGate = new Promise<void>((resolve) => {
      resolvePut = resolve
    })
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: ROOM,
              name: 'Ops',
              last_activity_ts: 200,
              last_event_id: '$latest',
              notification_count: 7,
              highlight_count: 0,
            },
          ],
        }),
      ),
      http.put(
        `${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`,
        () =>
          new Promise<Response>((resolve) => {
            putRequested = true
            void putGate.then(() =>
              resolve(
                HttpResponse.json({
                  data: { updated_at: '2026-07-20T12:00:00Z' },
                }),
              ),
            )
          }),
      ),
    )
    const services = testServices()
    const { findByRole } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    fireEvent.click(await findByRole('button', { name: 'Mark all as read' }))

    expect(await findByRole('button', { name: 'Marking…' })).toBeTruthy()
    await waitFor(() => expect(putRequested).toBe(true))
    expect(services.rooms.unreadCount(`${ACCOUNT}/${ROOM}`)).toBe(7)
    resolvePut()
    await waitFor(() =>
      expect(services.deviceState.readMarker(ACCOUNT, ROOM)).toEqual({
        eventId: '$latest',
        originTs: 200,
      }),
    )
    await waitFor(() =>
      expect(services.rooms.unreadCount(`${ACCOUNT}/${ROOM}`)).toBe(0),
    )
    expect(
      await findByRole('button', { name: 'Mark all as read' }),
    ).toBeTruthy()
  })

  it('clears unread badges when the mark-read write is requeued', async () => {
    vi.useFakeTimers()
    let puts = 0
    server.use(
      http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
        HttpResponse.json({
          data: [
            {
              account_id: ACCOUNT,
              account_user_id: '@me:hs',
              room_id: ROOM,
              name: 'Ops',
              last_activity_ts: 200,
              last_event_id: '$latest',
              notification_count: 4,
              highlight_count: 0,
            },
          ],
        }),
      ),
      http.put(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () => {
        puts += 1
        return HttpResponse.error()
      }),
    )
    const services = testServices()
    const { findByRole } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    fireEvent.click(await findByRole('button', { name: 'Mark all as read' }))

    await waitFor(() =>
      expect(services.deviceState.readMarker(ACCOUNT, ROOM)).toEqual({
        eventId: '$latest',
        originTs: 200,
      }),
    )
    await waitFor(() =>
      expect(services.rooms.unreadCount(`${ACCOUNT}/${ROOM}`)).toBe(0),
    )
    expect(puts).toBe(1)
    expect(
      await findByRole('button', { name: 'Mark all as read' }),
    ).toBeTruthy()
    vi.clearAllTimers()
    vi.useRealTimers()
  })

  it('shows the web client build version', () => {
    const services = testServices()
    const { getByText } = render(
      <ServicesContext.Provider value={services}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    expect(getByText(/Web client/)).toBeTruthy()
  })

  it('copies the web client version from the footer', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText },
    })
    const { getByRole, findByRole } = render(
      <ServicesContext.Provider value={testServices()}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    fireEvent.click(getByRole('button', { name: 'Copy web client version' }))
    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith(BUILD_INFO.displayVersion),
    )
    expect((await findByRole('status')).textContent).toBe('Copied')
  })

  it('copies the axon server version from server status', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText },
    })
    server.use(
      http.get(`${TEST_BASE_URL}/v1/status`, () =>
        HttpResponse.json({
          data: {
            backfill: { paused: false, free_bytes: 0, accounts: [] },
            build: {
              version: '0.15.0',
              git_hash: 'abcdef1234567890',
              profile: 'release',
              build_time: '2026-07-22T12:34:56Z',
              rustc_version: 'rustc 1.89.0',
            },
          },
        }),
      ),
    )
    const { findByRole } = render(
      <ServicesContext.Provider value={testServices()}>
        <SettingsPage />
      </ServicesContext.Provider>,
    )

    fireEvent.click(await findByRole('button', { name: 'Copy server status' }))
    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith(
        formatServerBuildLine({
          version: '0.15.0',
          git_hash: 'abcdef1234567890',
          profile: 'release',
          build_time: '2026-07-22T12:34:56Z',
          rustc_version: 'rustc 1.89.0',
        }),
      ),
    )
  })
})

function mockNavigatorProperty(
  name: keyof Navigator,
  value: unknown,
): () => void {
  const original =
    Object.getOwnPropertyDescriptor(Navigator.prototype, name) ??
    Object.getOwnPropertyDescriptor(navigator, name)
  Object.defineProperty(navigator, name, {
    configurable: true,
    get: () => value,
  })
  return () => {
    if (original !== undefined) {
      Object.defineProperty(navigator, name, original)
    } else {
      delete (navigator as unknown as Record<string, unknown>)[name]
    }
  }
}
