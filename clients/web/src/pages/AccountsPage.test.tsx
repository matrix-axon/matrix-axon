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
import { TEST_BASE_URL, testServices } from '../test/services'
import { AccountsPage } from './AccountsPage'

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

function renderPage(accounts: unknown[] = [ALICE, BOB]) {
  server.use(
    http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
      HttpResponse.json({ data: accounts }),
    ),
    http.get(`${TEST_BASE_URL}/v1/status`, () =>
      HttpResponse.json({ data: STATUS }),
    ),
  )
  const services: AppServices = testServices()
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
