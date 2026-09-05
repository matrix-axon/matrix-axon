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
import type { BrowserQrAdapter } from '../../qr/browser-qr'
import { ServicesContext, type AppServices } from '../../services'
import { memoryStorage } from '../../test/memory-storage'
import { TEST_BASE_URL, testServices } from '../../test/services'
import { AccountsPage } from '../AccountsPage'

const FLOW_ID = '10000000-0000-4000-8000-000000000001'
const ALICE = {
  account_id: '30000000-0000-4000-8000-000000000003',
  user_id: '@alice:example.org',
  homeserver_url: 'https://matrix.example.org',
  state: 'active',
  verified: true,
  backup: {
    exists_on_server: true,
    this_device_uploading: true,
    backup_state: 'enabled',
    recovery_state: 'enabled',
  },
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
} as const

const server = setupServer()
const renderedServices: AppServices[] = []

beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  for (const services of renderedServices) {
    services.matrixOAuthQr.reset()
    services.matrixOAuthQrGrant.reset()
  }
  renderedServices.length = 0
  cleanup()
  server.resetHandlers()
})
afterAll(() => server.close())

function qrAdapter(
  overrides: Partial<BrowserQrAdapter> = {},
): BrowserQrAdapter {
  return {
    decodeBase64: vi.fn(() => Uint8Array.from([0, 1, 254, 255])),
    encodeBase64: vi.fn(() => 'AAH-_w'),
    render: vi.fn(),
    scanImage: vi.fn(),
    listCameras: vi.fn().mockResolvedValue([]),
    watchCameras: vi.fn(() => () => {}),
    startCamera: vi.fn(),
    ...overrides,
  }
}

function renderPage(
  options: { qr?: BrowserQrAdapter; pendingStorage?: Storage } = {},
) {
  server.use(
    http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
      HttpResponse.json({ data: [ALICE] }),
    ),
    http.get(`${TEST_BASE_URL}/v1/status`, () =>
      HttpResponse.json({
        data: { backfill: { paused: false, accounts: [] } },
      }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/accounts/:accountId/users/:userId/profile`,
      ({ params }) =>
        HttpResponse.json({
          data: {
            user_id: String(params.userId),
            display_name: 'Alice Example',
            avatar_url: null,
          },
        }),
    ),
  )
  const services = testServices(options)
  renderedServices.push(services)
  const view = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <AccountsPage />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  return { services, ...view }
}

describe('MatrixOAuthQrGrant', () => {
  it('displays a binary QR for the selected account with explicit account guidance', async () => {
    let requestBody: unknown
    const qr = qrAdapter()
    const pendingStorage = memoryStorage()
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/login-grants/qr`,
        async ({ request }) => {
          requestBody = await request.json()
          return HttpResponse.json(
            {
              data: {
                flow_id: FLOW_ID,
                account_id: ALICE.account_id,
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
    const { findByRole, getByLabelText, getByRole } = renderPage({
      qr,
      pendingStorage,
    })

    fireEvent.click(
      await findByRole('button', { name: 'Set up device authorization' }),
    )
    expect(
      getByTextContent(/In the new client, sign in as @alice:example.org/),
    ).toBeTruthy()
    fireEvent.click(
      getByLabelText('Show a QR code for the new Matrix client to scan'),
    )
    fireEvent.click(getByRole('button', { name: 'Start device authorization' }))

    await waitFor(() =>
      expect(requestBody).toEqual({ presentation: 'display' }),
    )
    await waitFor(() =>
      expect(qr.render).toHaveBeenCalledWith(
        expect.any(HTMLCanvasElement),
        Uint8Array.from([0, 1, 254, 255]),
      ),
    )
    expect(
      getByRole('img', { name: 'Matrix device authorization QR code' }),
    ).toBeTruthy()
    expect(pendingStorage.length).toBe(1)
    expect(pendingStorage.key(0)).toBe('axon.matrix-oauth-qr-grant.flow-id')
    expect(pendingStorage.getItem('axon.matrix-oauth-qr-grant.flow-id')).toBe(
      FLOW_ID,
    )
  })

  it('falls back from camera to an image scan, displays the check code, and cancels', async () => {
    let scanBody: unknown
    let cancelled = false
    const qr = qrAdapter({
      startCamera: vi.fn().mockRejectedValue(new Error('camera denied')),
      scanImage: vi.fn().mockResolvedValue(Uint8Array.from([0, 1, 254, 255])),
    })
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/login-grants/qr`,
        () =>
          HttpResponse.json(
            {
              data: {
                flow_id: FLOW_ID,
                account_id: ALICE.account_id,
                presentation: 'scan',
                stage: 'starting',
              },
            },
            { status: 201 },
          ),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/login-grants/qr/${FLOW_ID}/scan`,
        async ({ request }) => {
          scanBody = await request.json()
          return HttpResponse.json({
            data: {
              flow_id: FLOW_ID,
              account_id: ALICE.account_id,
              presentation: 'scan',
              stage: 'check_code_to_display',
              check_code: '42',
            },
          })
        },
      ),
      http.delete(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/login-grants/qr/${FLOW_ID}`,
        () => {
          cancelled = true
          return new HttpResponse(null, { status: 204 })
        },
      ),
    )
    const { findByRole, findByText, getByLabelText, getByRole } = renderPage({
      qr,
    })

    fireEvent.click(
      await findByRole('button', { name: 'Set up device authorization' }),
    )
    fireEvent.click(getByRole('button', { name: 'Start device authorization' }))
    fireEvent.click(await findByText('Start camera'))
    expect(
      await findByText(/camera denied.*Choose an image instead/i),
    ).toBeTruthy()
    fireEvent.change(getByLabelText('Choose QR image'), {
      target: {
        files: [new File(['qr'], 'element-x.png', { type: 'image/png' })],
      },
    })

    await waitFor(() => expect(scanBody).toEqual({ qr_code_data: 'AAH-_w' }))
    expect(await findByText('42')).toBeTruthy()
    fireEvent.click(
      getByRole('button', { name: 'Cancel device authorization' }),
    )
    await waitFor(() => expect(cancelled).toBe(true))
    expect(await findByText('Device authorization cancelled.')).toBeTruthy()
  })

  it('submits a check code and requires explicit authorization-server approval', async () => {
    let checkBody: unknown
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/login-grants/qr`,
        () =>
          HttpResponse.json(
            {
              data: {
                flow_id: FLOW_ID,
                account_id: ALICE.account_id,
                presentation: 'display',
                stage: 'check_code_required',
              },
            },
            { status: 201 },
          ),
      ),
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/login-grants/qr/${FLOW_ID}/check-code`,
        async ({ request }) => {
          checkBody = await request.json()
          return HttpResponse.json({
            data: {
              flow_id: FLOW_ID,
              account_id: ALICE.account_id,
              presentation: 'display',
              stage: 'waiting_for_authorization',
              verification_uri: 'https://auth.example.org/device',
            },
          })
        },
      ),
    )
    const {
      findByLabelText,
      findByRole,
      findByText,
      getByLabelText,
      getByRole,
    } = renderPage()

    fireEvent.click(
      await findByRole('button', { name: 'Set up device authorization' }),
    )
    fireEvent.click(
      getByLabelText('Show a QR code for the new Matrix client to scan'),
    )
    fireEvent.click(getByRole('button', { name: 'Start device authorization' }))
    fireEvent.input(await findByLabelText('Two-digit check code'), {
      target: { value: '07' },
    })
    fireEvent.click(getByRole('button', { name: 'Confirm code' }))

    await waitFor(() => expect(checkBody).toEqual({ check_code: '07' }))
    expect(
      await findByText(/This flow cannot finish without that approval/),
    ).toBeTruthy()
    expect(
      getByRole('link', {
        name: 'Open the secure verification page',
      }).getAttribute('href'),
    ).toBe('https://auth.example.org/device')
  })

  it('recovers a terminal flow after reload and clears the persisted id', async () => {
    const pendingStorage = memoryStorage({
      'axon.matrix-oauth-qr-grant.flow-id': FLOW_ID,
    })
    server.use(
      http.get(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/login-grants/qr/${FLOW_ID}`,
        () =>
          HttpResponse.json({
            data: {
              flow_id: FLOW_ID,
              account_id: ALICE.account_id,
              presentation: 'scan',
              stage: 'failed',
              error_code: 'device_not_found',
            },
          }),
      ),
    )
    const { findByRole, findByText } = renderPage({ pendingStorage })

    expect(await findByText(/expected account/i)).toBeTruthy()
    expect(await findByRole('button', { name: 'Start again' })).toBeTruthy()
    expect(
      pendingStorage.getItem('axon.matrix-oauth-qr-grant.flow-id'),
    ).toBeNull()
  })

  it('cancels an active grant before the last account disappears', async () => {
    let cancelled = false
    server.use(
      http.post(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/login-grants/qr`,
        () =>
          HttpResponse.json(
            {
              data: {
                flow_id: FLOW_ID,
                account_id: ALICE.account_id,
                presentation: 'scan',
                stage: 'starting',
              },
            },
            { status: 201 },
          ),
      ),
      http.delete(
        `${TEST_BASE_URL}/v1/accounts/${ALICE.account_id}/login-grants/qr/${FLOW_ID}`,
        () => {
          cancelled = true
          return new HttpResponse(null, { status: 204 })
        },
      ),
    )
    const { services, findByRole, getByRole } = renderPage()

    fireEvent.click(
      await findByRole('button', { name: 'Set up device authorization' }),
    )
    fireEvent.click(getByRole('button', { name: 'Start device authorization' }))
    await waitFor(() =>
      expect(services.matrixOAuthQrGrant.flow.value).not.toBeNull(),
    )

    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    await services.accounts.refresh()

    await waitFor(() => expect(cancelled).toBe(true))
    expect(services.matrixOAuthQrGrant.flow.value?.stage).toBe('cancelled')
  })
})

function getByTextContent(pattern: RegExp): HTMLElement {
  const element = Array.from(document.querySelectorAll<HTMLElement>('p')).find(
    (candidate) => pattern.test(candidate.textContent ?? ''),
  )
  if (element === undefined) {
    throw new Error(`No paragraph matched ${pattern}`)
  }
  return element
}
