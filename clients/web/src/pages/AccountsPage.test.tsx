import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
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
import { ServicesContext, type AppServices } from '../services'
import type { BrowserQrAdapter } from '../qr/browser-qr'
import { TEST_BASE_URL, testServices } from '../test/services'
import { AccountsPage } from './AccountsPage'
import { formatServerBuildLine } from './ServerStatus'

const ALICE = {
  account_id: '6b53f7f0-0000-4000-8000-000000000001',
  user_id: '@alice:example.org',
  homeserver_url: 'https://matrix.example.org',
  state: 'active',
  verified: true,
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
}
const BOB = {
  ...ALICE,
  account_id: '6b53f7f0-0000-4000-8000-000000000002',
  user_id: '@bob:example.org',
  state: 'deactivated',
  verified: null,
}
const CAROL = {
  ...BOB,
  account_id: '6b53f7f0-0000-4000-8000-000000000003',
  user_id: '@carol:example.org',
}
const STATUS = {
  backfill: {
    paused: true,
    reason: 'low_disk',
    free_bytes: 2 * 1024 ** 3,
    accounts: [
      {
        account_id: ALICE.account_id,
        events: 1234,
        rooms_total: 10,
        rooms_backfilled: 7,
        complete: false,
      },
    ],
  },
}

// A structurally valid Matrix recovery key (see recovery-key.test.ts); the
// Recover button gates on this shape, so the form tests must use a real one.
const VALID_KEY = 'EsT1 t3bE JPZs Bz9H xApv jfQh PY9X gmGM bhbN Kz2L 2t9n aeKB'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  window.history.replaceState(null, '', '/')
  vi.unstubAllGlobals()
})
afterAll(() => server.close())

function renderPage(
  accounts: unknown[] | (() => unknown[]) = [ALICE, BOB],
  qr?: BrowserQrAdapter,
  status: unknown = STATUS,
) {
  server.use(
    http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
      HttpResponse.json({
        data: typeof accounts === 'function' ? accounts() : accounts,
      }),
    ),
    http.get(`${TEST_BASE_URL}/v1/status`, () =>
      HttpResponse.json({ data: status }),
    ),
  )
  const services: AppServices = testServices({ qr })
  const utils = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <AccountsPage />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  return { services, ...utils }
}

describe('AccountsPage', () => {
  it('lists accounts with state and verification badges', async () => {
    const { findByText, getByText } = renderPage()

    expect(await findByText('@alice:example.org')).toBeTruthy()
    expect(getByText('@bob:example.org')).toBeTruthy()
    expect(getByText('verified')).toBeTruthy()
    expect(getByText('deactivated')).toBeTruthy()
  })

  it('does not show sync readiness for a deactivated account', async () => {
    const { findByText, queryByText } = renderPage([
      { ...BOB, sync_state: 'connecting' },
    ])

    expect(await findByText('deactivated')).toBeTruthy()
    expect(queryByText('connecting')).toBeNull()
  })

  it('opens a focused, cancellable reactivation form from the account card', async () => {
    const { findByRole, getByLabelText, getByRole, queryByLabelText } =
      renderPage([BOB])

    fireEvent.click(await findByRole('button', { name: 'Sign in again' }))

    expect(
      getByRole('heading', { name: 'Reactivate @bob:example.org' }),
    ).toBeTruthy()
    const userId = getByLabelText(/Matrix user ID/) as HTMLInputElement
    const password = getByLabelText('Password') as HTMLInputElement
    expect(userId.value).toBe('@bob:example.org')
    expect(userId.readOnly).toBe(true)
    await waitFor(() => expect(document.activeElement).toBe(password))
    expect(queryByLabelText(/Homeserver URL/)).toBeNull()

    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    const expectedUserId = getByLabelText(
      'Expected Matrix user ID',
    ) as HTMLInputElement
    expect(expectedUserId.value).toBe('@bob:example.org')
    expect(expectedUserId.readOnly).toBe(true)

    fireEvent.click(getByRole('button', { name: 'Cancel reactivation' }))
    expect(getByRole('heading', { name: 'Add account' })).toBeTruthy()
    expect(
      (getByLabelText('Expected Matrix user ID') as HTMLInputElement).readOnly,
    ).toBe(false)
  })

  it('cancels an active QR flow before leaving reactivation', async () => {
    let cancellations = 0
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login/qr`, () =>
        HttpResponse.json(
          {
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: BOB.user_id,
              presentation: 'display',
              stage: 'waiting_for_authorization',
            },
          },
          { status: 201 },
        ),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/login/qr/:flowId`, () =>
        HttpResponse.json({
          data: {
            flow_id: '10000000-0000-4000-8000-000000000001',
            expected_user_id: BOB.user_id,
            presentation: 'display',
            stage: 'waiting_for_authorization',
          },
        }),
      ),
      http.delete(`${TEST_BASE_URL}/v1/accounts/login/qr/:flowId`, () => {
        cancellations += 1
        return new HttpResponse(null, { status: 204 })
      }),
    )
    const { findByRole, findByText, getByRole } = renderPage([BOB])

    fireEvent.click(await findByRole('button', { name: 'Sign in again' }))
    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))
    await findByText(/Approve the Matrix device authorization request/)
    fireEvent.click(getByRole('tab', { name: 'Sign in with password' }))
    fireEvent.click(getByRole('button', { name: 'Cancel reactivation' }))

    await waitFor(() => expect(cancellations).toBe(1))
    expect(getByRole('heading', { name: 'Add account' })).toBeTruthy()
  })

  it('keeps reactivation open when QR cancellation is unconfirmed', async () => {
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login/qr`, () =>
        HttpResponse.json(
          {
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: BOB.user_id,
              presentation: 'display',
              stage: 'waiting_for_authorization',
            },
          },
          { status: 201 },
        ),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/login/qr/:flowId`, () =>
        HttpResponse.json({
          data: {
            flow_id: '10000000-0000-4000-8000-000000000001',
            expected_user_id: BOB.user_id,
            presentation: 'display',
            stage: 'waiting_for_authorization',
          },
        }),
      ),
      http.delete(`${TEST_BASE_URL}/v1/accounts/login/qr/:flowId`, () =>
        HttpResponse.error(),
      ),
    )
    const { services, findByRole, findByText, getByRole } = renderPage([BOB])

    fireEvent.click(await findByRole('button', { name: 'Sign in again' }))
    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))
    await findByText(/Approve the Matrix device authorization request/)
    fireEvent.click(getByRole('tab', { name: 'Sign in with password' }))
    fireEvent.click(getByRole('button', { name: 'Cancel reactivation' }))

    expect(await findByText(/Cancellation was not confirmed/)).toBeTruthy()
    expect(
      getByRole('heading', { name: 'Reactivate @bob:example.org' }),
    ).toBeTruthy()
    services.matrixOAuthQr.reset()
  })

  it('does not show one account QR flow under another account heading', async () => {
    let cancellations = 0
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login/qr`, () =>
        HttpResponse.json(
          {
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: BOB.user_id,
              presentation: 'display',
              stage: 'waiting_for_authorization',
            },
          },
          { status: 201 },
        ),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/login/qr/:flowId`, () =>
        HttpResponse.json({
          data: {
            flow_id: '10000000-0000-4000-8000-000000000001',
            expected_user_id: BOB.user_id,
            presentation: 'display',
            stage: 'waiting_for_authorization',
          },
        }),
      ),
      http.delete(`${TEST_BASE_URL}/v1/accounts/login/qr/:flowId`, () => {
        cancellations += 1
        return new HttpResponse(null, { status: 204 })
      }),
    )
    const { services, findAllByRole, findByText, getByRole, queryByText } =
      renderPage([BOB, CAROL])

    const [reactivateBob, reactivateCarol] = await findAllByRole('button', {
      name: 'Sign in again',
    })
    fireEvent.click(reactivateBob)
    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))
    await findByText(/Approve the Matrix device authorization request/)

    fireEvent.click(reactivateCarol)
    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))

    expect(
      await findByText(/QR sign-in for @bob:example.org is still active/),
    ).toBeTruthy()
    expect(
      queryByText(/Approve the Matrix device authorization request/),
    ).toBeNull()
    fireEvent.click(getByRole('button', { name: 'Cancel previous QR sign-in' }))
    await waitFor(() => expect(cancellations).toBe(1))
    expect(
      (
        getByRole('textbox', {
          name: 'Expected Matrix user ID',
        }) as HTMLInputElement
      ).value,
    ).toBe(CAROL.user_id)
    services.matrixOAuthQr.reset()
  })

  it('reactivates with the stored identity and routes the first active account', async () => {
    window.history.replaceState(null, '', '/accounts')
    let reactivated = false
    let loginBody: unknown
    const { findByRole, getByLabelText, getByRole } = renderPage(() => [
      reactivated ? { ...BOB, state: 'active' } : BOB,
    ])
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login`, async ({ request }) => {
        loginBody = await request.json()
        reactivated = true
        return HttpResponse.json({ data: { ...BOB, state: 'active' } })
      }),
    )

    fireEvent.click(await findByRole('button', { name: 'Sign in again' }))
    fireEvent.input(getByLabelText('Password'), {
      target: { value: 'hunter2' },
    })
    fireEvent.click(getByRole('button', { name: 'Reactivate account' }))

    await waitFor(() =>
      expect(loginBody).toEqual({
        username: '@bob:example.org',
        password: 'hunter2',
        homeserver_url: null,
      }),
    )
    await waitFor(() => expect(window.location.pathname).toBe('/'))
  })

  it('copies user id, account id, and homeserver URL from a card', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText },
    })
    const { findAllByRole, getAllByRole } = renderPage()

    fireEvent.click(
      (await findAllByRole('button', { name: 'Copy user ID' }))[0],
    )
    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith('@alice:example.org'),
    )

    fireEvent.click(getAllByRole('button', { name: 'Copy account ID' })[0])
    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith(ALICE.account_id),
    )

    fireEvent.click(getAllByRole('button', { name: 'Copy homeserver URL' })[0])
    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith('https://matrix.example.org'),
    )
  })

  it('shows the backfill status including the paused warning', async () => {
    const { findByText, getByText } = renderPage()
    expect(await findByText('paused (low_disk)')).toBeTruthy()
    expect(getByText(/7\/10 rooms backfilled/)).toBeTruthy()
  })

  it('shows build and sync details from newer server status responses', async () => {
    const since = Date.UTC(2026, 6, 22, 12, 0, 0)
    const newerStatus = {
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
          account_id: ALICE.account_id,
          state: 'running',
          since_ms: since,
        },
      ],
    }
    const { container, findByRole, findByText, getByText } = renderPage(
      undefined,
      undefined,
      newerStatus,
    )

    expect(await findByText('0.15.0')).toBeTruthy()
    expect(getByText('abcdef123456')).toBeTruthy()
    expect(container.textContent).toContain('release')
    expect(getByText('rustc 1.89.0')).toBeTruthy()
    expect(await findByRole('heading', { name: 'Sync service' })).toBeTruthy()
    expect(getByText('running')).toBeTruthy()
    expect(getByText(ALICE.account_id.slice(0, 8))).toBeTruthy()
    expect(
      container.querySelector(
        `time[datetime="${new Date(since).toISOString()}"]`,
      ),
    ).toBeTruthy()
  })

  it('copies the axon server version from server status', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined)
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText },
    })
    const { findByRole } = renderPage(undefined, undefined, {
      backfill: { paused: false, free_bytes: 0, accounts: [] },
      build: {
        version: '0.15.0',
        git_hash: 'abcdef1234567890',
        profile: 'release',
        build_time: '2026-07-22T12:34:56Z',
        rustc_version: 'rustc 1.89.0',
      },
    })

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

  it('logs in through the add-account form', async () => {
    let loginBody: unknown
    let recoverBody: unknown
    const { findByText, getByLabelText, getByRole } = renderPage([])
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login`, async ({ request }) => {
        loginBody = await request.json()
        return HttpResponse.json({ data: ALICE })
      }),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/recover`,
        async ({ request }) => {
          recoverBody = await request.json()
          return HttpResponse.json({ data: { ...ALICE, verified: true } })
        },
      ),
    )
    await findByText('No accounts yet — add one below.')

    fireEvent.input(getByLabelText(/Matrix user ID/), {
      target: { value: '@alice:example.org' },
    })
    fireEvent.input(getByLabelText('Password'), {
      target: { value: 'hunter2' },
    })
    fireEvent.input(getByLabelText(/Matrix Recovery Key/), {
      target: { value: ` ${VALID_KEY} ` },
    })
    fireEvent.click(getByRole('button', { name: 'Log in' }))

    await waitFor(() =>
      expect(loginBody).toEqual({
        username: '@alice:example.org',
        password: 'hunter2',
        homeserver_url: null,
      }),
    )
    expect(recoverBody).toEqual({ recovery_key: VALID_KEY })
    await waitFor(() => expect(window.location.pathname).toBe('/'))
  })

  it('does not leave Accounts when adding another account later', async () => {
    window.history.replaceState(null, '', '/accounts')
    let loginBody: unknown
    const { findByText, getByLabelText, getByRole } = renderPage([ALICE])
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login`, async ({ request }) => {
        loginBody = await request.json()
        return HttpResponse.json({
          data: { ...BOB, state: 'active', verified: true },
        })
      }),
    )

    await findByText('@alice:example.org')

    fireEvent.input(getByLabelText(/Matrix user ID/), {
      target: { value: '@bob:example.org' },
    })
    fireEvent.input(getByLabelText('Password'), {
      target: { value: 'hunter2' },
    })
    fireEvent.click(getByRole('button', { name: 'Log in' }))

    await waitFor(() =>
      expect(loginBody).toEqual({
        username: '@bob:example.org',
        password: 'hunter2',
        homeserver_url: null,
      }),
    )
    expect(window.location.pathname).toBe('/accounts')
  })

  it('accepts login without a recovery key but blocks a malformed one', async () => {
    const { findByText, getByLabelText, getByRole, queryByText } = renderPage(
      [],
    )
    await findByText('No accounts yet — add one below.')

    fireEvent.input(getByLabelText(/Matrix user ID/), {
      target: { value: '@alice:example.org' },
    })
    fireEvent.input(getByLabelText('Password'), {
      target: { value: 'hunter2' },
    })
    const login = getByRole('button', { name: 'Log in' }) as HTMLButtonElement
    // Optional field left blank: login stays enabled.
    expect(login.disabled).toBe(false)
    expect(queryByText(/valid recovery key/i)).toBeNull()

    // A non-empty malformed key blocks submit and hints.
    fireEvent.input(getByLabelText(/Matrix Recovery Key/), {
      target: { value: 'not-a-real-key' },
    })
    expect(login.disabled).toBe(true)
    expect(queryByText(/valid recovery key/i)).toBeTruthy()

    // Clearing it again re-enables (still optional).
    fireEvent.input(getByLabelText(/Matrix Recovery Key/), {
      target: { value: '' },
    })
    expect(login.disabled).toBe(false)
    expect(queryByText(/valid recovery key/i)).toBeNull()
  })

  it('starts the display QR method and renders the decoded binary payload', async () => {
    let startBody: unknown
    const renderQr = vi.fn().mockResolvedValue(undefined)
    const qr: BrowserQrAdapter = {
      decodeBase64: vi.fn(() => Uint8Array.from([0, 1, 254, 255])),
      encodeBase64: vi.fn(),
      render: renderQr,
      scanImage: vi.fn(),
      listCameras: vi.fn().mockResolvedValue([]),
      watchCameras: vi.fn(() => () => {}),
      startCamera: vi.fn(),
    }
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/login/qr`,
        async ({ request }) => {
          startBody = await request.json()
          return HttpResponse.json(
            {
              data: {
                flow_id: '10000000-0000-4000-8000-000000000001',
                expected_user_id: '@alice:example.org',
                presentation: 'display',
                stage: 'qr_ready',
                qr_code_data: 'AAH-_w',
              },
            },
            { status: 201 },
          )
        },
      ),
    )
    const { services, findByRole, getByLabelText, getByRole } = renderPage(
      [],
      qr,
    )

    fireEvent.click(await findByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.input(getByLabelText('Expected Matrix user ID'), {
      target: { value: '@alice:example.org' },
    })
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))

    await waitFor(() =>
      expect(startBody).toEqual({
        expected_user_id: '@alice:example.org',
        presentation: 'display',
      }),
    )
    await waitFor(() =>
      expect(renderQr).toHaveBeenCalledWith(
        expect.any(HTMLCanvasElement),
        Uint8Array.from([0, 1, 254, 255]),
      ),
    )
    expect(getByRole('img', { name: 'Matrix sign-in QR code' })).toBeTruthy()
    services.matrixOAuthQr.reset()
  })

  it('shows malformed display QR data as a recoverable render error', async () => {
    const renderQr = vi.fn()
    const qr: BrowserQrAdapter = {
      decodeBase64: vi.fn(() => {
        throw new Error('QR data is not valid base64')
      }),
      encodeBase64: vi.fn(),
      render: renderQr,
      scanImage: vi.fn(),
      listCameras: vi.fn().mockResolvedValue([]),
      watchCameras: vi.fn(() => () => {}),
      startCamera: vi.fn(),
    }
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login/qr`, () =>
        HttpResponse.json(
          {
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: '@alice:example.org',
              presentation: 'display',
              stage: 'qr_ready',
              qr_code_data: 'malformed',
            },
          },
          { status: 201 },
        ),
      ),
    )
    const { services, findByText, getByLabelText, getByRole } = renderPage(
      [],
      qr,
    )

    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.input(getByLabelText('Expected Matrix user ID'), {
      target: { value: '@alice:example.org' },
    })
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))

    expect(await findByText('QR data is not valid base64')).toBeTruthy()
    expect(renderQr).not.toHaveBeenCalled()
    services.matrixOAuthQr.reset()
  })

  it('falls back from camera failure to image scan and submits exact base64', async () => {
    let scanBody: unknown
    const qr: BrowserQrAdapter = {
      decodeBase64: vi.fn(),
      encodeBase64: vi.fn(() => 'AAH-_w'),
      render: vi.fn(),
      scanImage: vi.fn().mockResolvedValue(Uint8Array.from([0, 1, 254, 255])),
      listCameras: vi.fn().mockResolvedValue([]),
      watchCameras: vi.fn(() => () => {}),
      startCamera: vi.fn().mockRejectedValue(new Error('camera denied')),
    }
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login/qr`, () =>
        HttpResponse.json(
          {
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: '@alice:example.org',
              presentation: 'scan',
              stage: 'starting',
            },
          },
          { status: 201 },
        ),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/login/qr/:flowId/scan`,
        async ({ request }) => {
          scanBody = await request.json()
          return HttpResponse.json({
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: '@alice:example.org',
              presentation: 'scan',
              stage: 'check_code_to_display',
              check_code: '42',
            },
          })
        },
      ),
    )
    const { services, findByText, getByLabelText, getByRole } = renderPage(
      [],
      qr,
    )

    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.input(getByLabelText('Expected Matrix user ID'), {
      target: { value: '@alice:example.org' },
    })
    fireEvent.click(getByLabelText('Scan a QR code with this device'))
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))
    fireEvent.click(await findByText('Start camera'))
    expect(
      await findByText(/camera denied.*Choose an image instead/i),
    ).toBeTruthy()

    fireEvent.change(getByLabelText('Choose QR image'), {
      target: {
        files: [new File(['qr'], 'qr.png', { type: 'image/png' })],
      },
    })
    await waitFor(() => expect(scanBody).toEqual({ qr_code_data: 'AAH-_w' }))
    expect(await findByText('42')).toBeTruthy()
    services.matrixOAuthQr.reset()
  })

  it('lists available cameras and releases the old stream when switching', async () => {
    const stopRear = vi.fn()
    const stopUsb = vi.fn()
    const startCamera = vi
      .fn()
      .mockResolvedValueOnce({ deviceId: 'rear', stop: stopRear })
      .mockResolvedValueOnce({ deviceId: 'usb', stop: stopUsb })
    const qr: BrowserQrAdapter = {
      decodeBase64: vi.fn(),
      encodeBase64: vi.fn(),
      render: vi.fn(),
      scanImage: vi.fn(),
      listCameras: vi.fn().mockResolvedValue([
        { deviceId: 'rear', label: 'Built-in rear camera' },
        { deviceId: 'usb', label: 'USB document camera' },
      ]),
      watchCameras: vi.fn(() => () => {}),
      startCamera,
    }
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login/qr`, () =>
        HttpResponse.json(
          {
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: '@alice:example.org',
              presentation: 'scan',
              stage: 'starting',
            },
          },
          { status: 201 },
        ),
      ),
    )
    const { findByLabelText, findByText, getByLabelText, getByRole, unmount } =
      renderPage([], qr)

    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.input(getByLabelText('Expected Matrix user ID'), {
      target: { value: '@alice:example.org' },
    })
    fireEvent.click(getByLabelText('Scan a QR code with this device'))
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))
    fireEvent.click(await findByText('Start camera'))

    const picker = (await findByLabelText('Camera')) as HTMLSelectElement
    expect(picker.value).toBe('rear')
    expect(getByRole('option', { name: 'Built-in rear camera' })).toBeTruthy()
    expect(getByRole('option', { name: 'USB document camera' })).toBeTruthy()
    expect(startCamera).toHaveBeenNthCalledWith(
      1,
      expect.any(HTMLVideoElement),
      expect.any(Function),
      expect.any(Function),
      undefined,
    )

    fireEvent.change(picker, { target: { value: 'usb' } })
    await waitFor(() =>
      expect(startCamera).toHaveBeenNthCalledWith(
        2,
        expect.any(HTMLVideoElement),
        expect.any(Function),
        expect.any(Function),
        'usb',
      ),
    )
    expect(stopRear).toHaveBeenCalledOnce()
    expect(picker.value).toBe('usb')

    unmount()
    expect(stopUsb).toHaveBeenCalledOnce()
  })

  it('accepts exactly two check-code digits and exposes only a safe approval link', async () => {
    let checkBody: unknown
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login/qr`, () =>
        HttpResponse.json(
          {
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: '@alice:example.org',
              presentation: 'display',
              stage: 'check_code_required',
            },
          },
          { status: 201 },
        ),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/login/qr/:flowId/check-code`,
        async ({ request }) => {
          checkBody = await request.json()
          return HttpResponse.json({
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: '@alice:example.org',
              presentation: 'display',
              stage: 'waiting_for_authorization',
              authorization_user_code: 'ABCD-EFGH',
              verification_uri: 'https://auth.example.org/device',
            },
          })
        },
      ),
    )
    const {
      container,
      services,
      findByLabelText,
      findByRole,
      getAllByRole,
      getByLabelText,
      getByRole,
    } = renderPage([])

    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.input(getByLabelText('Expected Matrix user ID'), {
      target: { value: '@alice:example.org' },
    })
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))
    const input = await findByLabelText('Two-digit check code')
    expect(
      getAllByRole('textbox', { name: 'Two-digit check code' }),
    ).toHaveLength(1)
    const cells = container.querySelectorAll('.segmented-code-cell')
    expect(cells).toHaveLength(2)
    const confirm = getByRole('button', {
      name: 'Confirm code',
    }) as HTMLButtonElement
    fireEvent.input(input, { target: { value: '1a' } })
    expect(cells[0].textContent).toBe('1')
    expect(cells[1].textContent).toBe('')
    expect(confirm.disabled).toBe(true)
    fireEvent.input(input, { target: { value: '12' } })
    expect(cells[0].textContent).toBe('1')
    expect(cells[1].textContent).toBe('2')
    expect(confirm.disabled).toBe(false)
    fireEvent.click(confirm)

    await waitFor(() => expect(checkBody).toEqual({ check_code: '12' }))
    expect(
      await findByRole('button', { name: 'Copy authorization user code' }),
    ).toBeTruthy()
    expect(
      getByRole('link', { name: 'Open the secure verification page' }),
    ).toHaveProperty('protocol', 'https:')
    services.matrixOAuthQr.reset()
  })

  it('renders terminal QR failure and cancellation recovery controls', async () => {
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login/qr`, () =>
        HttpResponse.json(
          {
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: '@alice:example.org',
              presentation: 'display',
              stage: 'failed',
              error_code: 'rendezvous_expired',
            },
          },
          { status: 201 },
        ),
      ),
    )
    const { findByText, getByLabelText, getByRole } = renderPage([])

    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.input(getByLabelText('Expected Matrix user ID'), {
      target: { value: '@alice:example.org' },
    })
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))

    expect(await findByText(/rendezvous expired/i)).toBeTruthy()
    fireEvent.click(getByRole('button', { name: 'Start again' }))
    expect(getByRole('button', { name: 'Start QR sign-in' })).toBeTruthy()
  })

  it('routes the first completed QR acquisition to the room index', async () => {
    window.history.replaceState(null, '', '/accounts')
    let accountLoads = 0
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login/qr`, () =>
        HttpResponse.json(
          {
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: '@alice:example.org',
              presentation: 'display',
              stage: 'done',
              account_id: ALICE.account_id,
            },
          },
          { status: 201 },
        ),
      ),
    )
    const { findByText, getByLabelText, getByRole } = renderPage(() =>
      accountLoads++ === 0 ? [] : [ALICE],
    )

    await findByText('No accounts yet — add one below.')
    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.input(getByLabelText('Expected Matrix user ID'), {
      target: { value: '@alice:example.org' },
    })
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))

    await waitFor(() => expect(window.location.pathname).toBe('/'))
  })

  it('waits for the completed QR account refresh before navigating', async () => {
    window.history.replaceState(null, '', '/accounts')
    let refreshRequested = false
    let releaseRefresh: (() => void) | undefined
    const refreshBlocked = new Promise<void>((resolve) => {
      releaseRefresh = resolve
    })
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login/qr`, () =>
        HttpResponse.json(
          {
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: '@alice:example.org',
              presentation: 'display',
              stage: 'done',
              account_id: ALICE.account_id,
            },
          },
          { status: 201 },
        ),
      ),
    )
    const { findByText, getByLabelText, getByRole } = renderPage([])
    await findByText('No accounts yet — add one below.')
    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts`, async () => {
        refreshRequested = true
        await refreshBlocked
        return HttpResponse.json({ data: [ALICE] })
      }),
    )

    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.input(getByLabelText('Expected Matrix user ID'), {
      target: { value: '@alice:example.org' },
    })
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))

    await waitFor(() => expect(refreshRequested).toBe(true))
    const pathWhileRefreshing = window.location.pathname
    releaseRefresh?.()
    await waitFor(() => expect(window.location.pathname).toBe('/'))
    expect(pathWhileRefreshing).toBe('/accounts')
  })

  it('reactivates by QR and routes when only deactivated accounts existed', async () => {
    window.history.replaceState(null, '', '/accounts')
    const carol = {
      ...BOB,
      account_id: '6b53f7f0-0000-4000-8000-000000000003',
      user_id: '@carol:example.org',
    }
    let reactivated = false
    let startBody: unknown
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/login/qr`,
        async ({ request }) => {
          startBody = await request.json()
          reactivated = true
          return HttpResponse.json(
            {
              data: {
                flow_id: '10000000-0000-4000-8000-000000000001',
                expected_user_id: BOB.user_id,
                presentation: 'display',
                stage: 'done',
                account_id: BOB.account_id,
              },
            },
            { status: 201 },
          )
        },
      ),
    )
    const { findAllByRole, getByRole } = renderPage(() => [
      reactivated ? { ...BOB, state: 'active' } : BOB,
      carol,
    ])

    const [reactivateBob] = await findAllByRole('button', {
      name: 'Sign in again',
    })
    fireEvent.click(reactivateBob)
    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))

    await waitFor(() =>
      expect(startBody).toEqual({
        expected_user_id: '@bob:example.org',
        presentation: 'display',
      }),
    )
    await waitFor(() => expect(window.location.pathname).toBe('/'))
  })

  it('stays on Accounts after a subsequent completed QR acquisition', async () => {
    window.history.replaceState(null, '', '/accounts')
    server.use(
      http.post(`${TEST_BASE_URL}/v1/accounts/login/qr`, () =>
        HttpResponse.json(
          {
            data: {
              flow_id: '10000000-0000-4000-8000-000000000001',
              expected_user_id: '@bob:example.org',
              presentation: 'display',
              stage: 'done',
              account_id: BOB.account_id,
            },
          },
          { status: 201 },
        ),
      ),
    )
    const { findByText, getByLabelText, getByRole } = renderPage([ALICE, BOB])

    await findByText('@alice:example.org')
    fireEvent.click(getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.input(getByLabelText('Expected Matrix user ID'), {
      target: { value: '@bob:example.org' },
    })
    fireEvent.click(getByRole('button', { name: 'Start QR sign-in' }))

    expect(await findByText(/Account signed in.*verified/i)).toBeTruthy()
    expect(window.location.pathname).toBe('/accounts')
    fireEvent.click(getByRole('button', { name: 'Add another account' }))
    expect(getByRole('button', { name: 'Start QR sign-in' })).toBeTruthy()
  })

  it('delete requires a confirmation step', async () => {
    let deleted = false
    const { findAllByRole, getByRole, queryByRole } = renderPage([ALICE])
    server.use(
      http.delete(`${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}`, () => {
        deleted = true
        return new HttpResponse(null, { status: 204 })
      }),
    )

    const [deleteButton] = await findAllByRole('button', { name: 'Delete' })
    fireEvent.click(deleteButton)
    expect(deleted).toBe(false)

    // Cancel first: nothing happens.
    fireEvent.click(getByRole('button', { name: 'Cancel' }))
    expect(queryByRole('button', { name: 'Confirm delete' })).toBeNull()

    fireEvent.click(getByRole('button', { name: 'Delete' }))
    fireEvent.click(getByRole('button', { name: 'Confirm delete' }))
    await waitFor(() => expect(deleted).toBe(true))
  })

  it('logout posts to the account-scoped route', async () => {
    let loggedOut = false
    const { findByRole } = renderPage([ALICE])
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/logout`,
        () => {
          loggedOut = true
          return HttpResponse.json({ data: { ...ALICE, state: 'deactivated' } })
        },
      ),
    )

    fireEvent.click(await findByRole('button', { name: 'Log out' }))
    await waitFor(() => expect(loggedOut).toBe(true))
  })

  it('recover reveals the form, submits a valid key, and reports success', async () => {
    let recoverBody: unknown
    const { findByRole, getByLabelText, getByRole, findByText } = renderPage([
      ALICE,
    ])
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/recover`,
        async ({ request }) => {
          recoverBody = await request.json()
          return HttpResponse.json({ data: { ...ALICE, verified: true } })
        },
      ),
    )

    fireEvent.click(await findByRole('button', { name: 'Recover keys' }))
    fireEvent.input(getByLabelText('Recovery key'), {
      target: { value: VALID_KEY },
    })
    fireEvent.click(getByRole('button', { name: 'Recover' }))

    await waitFor(() =>
      expect(recoverBody).toEqual({ recovery_key: VALID_KEY }),
    )
    // Success surfaces an inline notice; the form input is gone.
    expect(await findByText(/this device is now verified/i)).toBeTruthy()
  })

  it('reports keys imported but device still unverified', async () => {
    const { findByRole, getByLabelText, getByRole, findByText } = renderPage([
      ALICE,
    ])
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/recover`,
        () => HttpResponse.json({ data: { ...ALICE, verified: false } }),
      ),
    )

    fireEvent.click(await findByRole('button', { name: 'Recover keys' }))
    fireEvent.input(getByLabelText('Recovery key'), {
      target: { value: VALID_KEY },
    })
    fireEvent.click(getByRole('button', { name: 'Recover' }))

    expect(await findByText(/still unverified/i)).toBeTruthy()
  })

  it('gates the Recover button and hints on a malformed key', async () => {
    const { findByRole, getByLabelText, getByRole, queryByText } = renderPage([
      ALICE,
    ])

    fireEvent.click(await findByRole('button', { name: 'Recover keys' }))
    const recover = getByRole('button', {
      name: 'Recover',
    }) as HTMLButtonElement
    // Empty: disabled, no hint yet (an untouched field is not an error).
    expect(recover.disabled).toBe(true)
    expect(queryByText(/valid recovery key/i)).toBeNull()

    fireEvent.input(getByLabelText('Recovery key'), {
      target: { value: 'not-a-real-key' },
    })
    expect(recover.disabled).toBe(true)
    expect(queryByText(/valid recovery key/i)).toBeTruthy()

    fireEvent.input(getByLabelText('Recovery key'), {
      target: { value: VALID_KEY },
    })
    expect(recover.disabled).toBe(false)
    expect(queryByText(/valid recovery key/i)).toBeNull()
  })

  it('the account switch persists to settings', async () => {
    const { services, findAllByLabelText } = renderPage([ALICE, BOB])
    const [radio] = await findAllByLabelText('use this account')

    fireEvent.click(radio)

    expect(services.settings.activeAccountId.value).toBe(ALICE.account_id)
  })

  it('surfaces and dismisses a load error', async () => {
    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
        HttpResponse.json(
          { error: { code: 'internal', message: 'database unavailable' } },
          { status: 500 },
        ),
      ),
      http.get(`${TEST_BASE_URL}/v1/status`, () =>
        HttpResponse.json({ data: STATUS }),
      ),
    )
    const services = testServices()
    const { findByRole, getByRole, queryByRole } = render(
      <ServicesContext.Provider value={services}>
        <AccountsPage />
      </ServicesContext.Provider>,
    )

    expect((await findByRole('alert')).textContent).toContain(
      'database unavailable',
    )
    fireEvent.click(getByRole('button', { name: 'Dismiss' }))
    expect(queryByRole('alert')).toBeNull()
  })
})
