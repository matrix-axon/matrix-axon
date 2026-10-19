import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { LocationProvider } from 'preact-iso'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { ServicesContext, type AppServices } from '../../services'
import { memoryStorage } from '../../test/memory-storage'
import {
  TEST_BASE_URL,
  UNKNOWN_BACKUP,
  testServices,
} from '../../test/services'
import { AccountsPage } from '../AccountsPage'

const FLOW_ID = '10000000-0000-4000-8000-000000000001'
const FLOW_URL = `${TEST_BASE_URL}/v1/accounts/login/browser/${FLOW_ID}`
const CREATE_URL = `${TEST_BASE_URL}/v1/accounts/login/browser`
const STORAGE_KEY = 'axon.matrix-oauth-browser.flow-id'
const LINK = 'https://auth.example.org/link?code=ABCD-EFGH'

const ALICE = {
  account_id: '6b53f7f0-0000-4000-8000-000000000001',
  user_id: '@alice:example.org',
  homeserver_url: 'https://matrix.example.org',
  device_id: 'AXONDEV',
  state: 'active',
  verified: false,
  backup: UNKNOWN_BACKUP,
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
}
const BOB = {
  ...ALICE,
  account_id: '6b53f7f0-0000-4000-8000-000000000002',
  user_id: '@bob:example.org',
  device_id: null,
  state: 'deactivated',
  verified: null,
}

function flow(stage: string, extra: Record<string, unknown> = {}) {
  return {
    data: {
      flow_id: FLOW_ID,
      expected_user_id: ALICE.user_id,
      stage,
      ...extra,
    },
  }
}

const WAITING = flow('waiting_for_authorization', {
  verification_uri: LINK,
  user_code: 'ABCD-EFGH',
})

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  window.history.replaceState(null, '', '/')
})
afterAll(() => server.close())

function renderPage(
  accounts: unknown[] | (() => unknown[]) = [],
  pendingStorage?: Storage,
) {
  window.history.replaceState(null, '', '/accounts')
  server.use(
    http.get(`${TEST_BASE_URL}/v1/accounts`, () =>
      HttpResponse.json({
        data: typeof accounts === 'function' ? accounts() : accounts,
      }),
    ),
    http.get(`${TEST_BASE_URL}/v1/status`, () =>
      HttpResponse.json({
        data: { backfill: { paused: false, free_bytes: 0, accounts: [] } },
      }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/accounts/:accountId/users/:userId/profile`,
      ({ params }) =>
        HttpResponse.json({
          data: {
            user_id: String(params.userId),
            display_name: null,
            avatar_url: null,
          },
        }),
    ),
    http.get(`${TEST_BASE_URL}/v1/accounts/:accountId/devices`, () =>
      HttpResponse.json({ data: { user_id: ALICE.user_id, devices: [] } }),
    ),
  )
  const services: AppServices = testServices({ pendingStorage })
  const utils = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <AccountsPage />
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  return { services, ...utils }
}

async function start(
  page: ReturnType<typeof renderPage>,
  userId = ALICE.user_id,
) {
  await page.findByRole('tab', { name: 'Sign in with browser' })
  fireEvent.click(page.getByRole('tab', { name: 'Sign in with browser' }))
  fireEvent.input(page.getByLabelText('Matrix user ID'), {
    target: { value: userId },
  })
  fireEvent.click(page.getByRole('button', { name: 'Start browser sign-in' }))
}

describe('browser sign-in on the Accounts page', () => {
  it('shows a link the user opens in a new tab, its host, and the code', async () => {
    let created: unknown = null
    server.use(
      http.post(CREATE_URL, async ({ request }) => {
        created = await request.json()
        return HttpResponse.json(WAITING, { status: 201 })
      }),
    )
    const page = renderPage()
    await start(page)

    const link = await page.findByRole('link', { name: 'Open sign-in page' })
    expect(created).toEqual({ expected_user_id: ALICE.user_id })
    expect(link.getAttribute('href')).toBe(LINK)
    expect(link.getAttribute('target')).toBe('_blank')
    expect(link.getAttribute('rel')).toBe('noopener noreferrer')
    expect(page.getByText('auth.example.org')).toBeTruthy()
    // Shown even though this link carries it: a bare link needs it typed.
    expect(page.getByText('ABCD-EFGH')).toBeTruthy()
    expect(page.getByRole('button', { name: 'Cancel sign-in' })).toBeTruthy()
  })

  it('refuses to render a link that is not HTTPS', async () => {
    server.use(
      http.post(CREATE_URL, () =>
        HttpResponse.json(
          flow('waiting_for_authorization', {
            verification_uri: 'javascript:alert(1)',
            user_code: 'ABCD-EFGH',
          }),
          { status: 201 },
        ),
      ),
    )
    const page = renderPage()
    await start(page)

    await page.findByText(/unsafe sign-in link/)
    expect(page.queryByRole('link', { name: 'Open sign-in page' })).toBeNull()
  })

  it('stays on Accounts after a first sign-in and leads on to verification', async () => {
    let signedIn = false
    server.use(
      http.post(CREATE_URL, () => {
        signedIn = true
        return HttpResponse.json(
          flow('done', { account_id: ALICE.account_id }),
          { status: 201 },
        )
      }),
    )
    const page = renderPage(() => (signedIn ? [ALICE] : []))
    await page.findByText('No accounts yet — add one below.')
    await start(page)

    await page.findByText(/not verified yet/)
    // QR sign-in leaves for the room list here; this must not, or the user
    // lands in rooms that cannot decrypt with no hint why.
    expect(window.location.pathname).toBe('/accounts')

    fireEvent.click(
      page.getByRole('button', { name: 'Verify with another device' }),
    )
    expect(page.services.verification.picker.value).toEqual({
      accountId: ALICE.account_id,
      ownDeviceId: 'AXONDEV',
    })

    fireEvent.click(page.getByRole('button', { name: 'Go to rooms' }))
    await waitFor(() => expect(window.location.pathname).toBe('/'))
  })

  it('offers no shortcut to rooms when another account was already active', async () => {
    const carol = {
      ...ALICE,
      account_id: '6b53f7f0-0000-4000-8000-000000000003',
      user_id: '@carol:example.org',
    }
    server.use(
      http.post(CREATE_URL, () =>
        HttpResponse.json(flow('done', { account_id: ALICE.account_id }), {
          status: 201,
        }),
      ),
    )
    const page = renderPage([carol, ALICE])
    await page.findByText('@carol:example.org')
    await start(page)

    await page.findByText(/not verified yet/)
    expect(page.queryByRole('button', { name: 'Go to rooms' })).toBeNull()
    fireEvent.click(page.getByRole('button', { name: 'Add another account' }))
    await page.findByRole('button', { name: 'Start browser sign-in' })
    // Blank for the next account, as after a password sign-in.
    expect(
      (page.getByLabelText('Matrix user ID') as HTMLInputElement).value,
    ).toBe('')
  })

  it('explains a refusal and lets the user start again', async () => {
    server.use(
      http.post(CREATE_URL, () =>
        HttpResponse.json(
          flow('failed', { error_code: 'authorization_denied' }),
          { status: 201 },
        ),
      ),
    )
    const page = renderPage()
    await start(page)

    await page.findByText(/declined in the browser/)
    fireEvent.click(page.getByRole('button', { name: 'Start again' }))
    await page.findByRole('button', { name: 'Start browser sign-in' })
  })

  it('returns to a sign-in that a reload interrupted', async () => {
    server.use(http.get(FLOW_URL, () => HttpResponse.json(WAITING)))
    const page = renderPage([], memoryStorage({ [STORAGE_KEY]: FLOW_ID }))

    // No tab was clicked: the stored flow selects it.
    await page.findByRole('link', { name: 'Open sign-in page' })
    expect(
      page
        .getByRole('tab', { name: 'Sign in with browser' })
        .getAttribute('aria-selected'),
    ).toBe('true')
  })

  it('says when a newer sign-in for the same user replaced this one', async () => {
    server.use(
      http.post(CREATE_URL, () => HttpResponse.json(WAITING, { status: 201 })),
      http.get(FLOW_URL, () =>
        HttpResponse.json(flow('cancelled', { cancel_reason: 'replaced' })),
      ),
    )
    const page = renderPage()
    await start(page)
    await page.findByRole('link', { name: 'Open sign-in page' })

    document.dispatchEvent(new Event('visibilitychange'))

    await page.findByText(/A newer browser sign-in for this account replaced/)
    expect(page.queryByText('Browser sign-in cancelled.')).toBeNull()
    expect(page.queryByRole('link', { name: 'Open sign-in page' })).toBeNull()
    fireEvent.click(page.getByRole('button', { name: 'Start again' }))
    await page.findByRole('button', { name: 'Start browser sign-in' })
  })

  it('asks again as soon as the user comes back to the page', async () => {
    let polls = 0
    server.use(
      http.post(CREATE_URL, () => HttpResponse.json(WAITING, { status: 201 })),
      http.get(FLOW_URL, () => {
        polls += 1
        return HttpResponse.json(flow('finalizing'))
      }),
    )
    const page = renderPage()
    await start(page)
    await page.findByRole('link', { name: 'Open sign-in page' })
    expect(polls).toBe(0)

    document.dispatchEvent(new Event('visibilitychange'))

    await page.findByText('Approved. Finishing sign-in…')
    expect(polls).toBe(1)
    // The link and code are gone with the stage that carried them.
    expect(page.queryByRole('link', { name: 'Open sign-in page' })).toBeNull()
    expect(page.queryByText('ABCD-EFGH')).toBeNull()
  })

  it('reactivates with the stored user ID and cancels the flow on leaving', async () => {
    let cancelled = false
    let created: unknown = null
    server.use(
      http.post(CREATE_URL, async ({ request }) => {
        created = await request.json()
        return HttpResponse.json(
          {
            data: { ...WAITING.data, expected_user_id: BOB.user_id },
          },
          { status: 201 },
        )
      }),
      http.delete(FLOW_URL, () => {
        cancelled = true
        return new HttpResponse(null, { status: 204 })
      }),
      http.get(FLOW_URL, () =>
        HttpResponse.json({
          data: {
            flow_id: FLOW_ID,
            expected_user_id: BOB.user_id,
            stage: cancelled ? 'cancelled' : 'waiting_for_authorization',
          },
        }),
      ),
    )
    const page = renderPage([BOB])
    fireEvent.click(await page.findByRole('button', { name: 'Sign in again' }))
    fireEvent.click(page.getByRole('tab', { name: 'Sign in with browser' }))
    const input = page.getByLabelText('Matrix user ID') as HTMLInputElement
    expect(input.value).toBe(BOB.user_id)
    expect(input.readOnly).toBe(true)
    fireEvent.click(page.getByRole('button', { name: 'Start browser sign-in' }))
    await page.findByRole('link', { name: 'Open sign-in page' })
    expect(created).toEqual({ expected_user_id: BOB.user_id })

    fireEvent.click(page.getByRole('button', { name: 'Cancel reactivation' }))

    await page.findByRole('heading', { name: 'Add account' })
    expect(cancelled).toBe(true)
    expect(page.services.matrixOAuthBrowser.flow.value).toBeNull()
  })
  it('finishes the reactivation when the sign-in beat the cancellation', async () => {
    let state = 'deactivated'
    const bob = () => [{ ...BOB, state, device_id: 'AXONDEV', verified: false }]
    server.use(
      http.post(CREATE_URL, () =>
        HttpResponse.json(
          { data: { ...WAITING.data, expected_user_id: BOB.user_id } },
          { status: 201 },
        ),
      ),
      // As Axon does once a sign-in is being finished: accept, change nothing.
      http.delete(FLOW_URL, () => {
        state = 'active'
        return new HttpResponse(null, { status: 204 })
      }),
      http.get(FLOW_URL, () =>
        HttpResponse.json({
          data: {
            flow_id: FLOW_ID,
            expected_user_id: BOB.user_id,
            stage: 'done',
            account_id: BOB.account_id,
          },
        }),
      ),
    )
    const page = renderPage(bob)
    fireEvent.click(await page.findByRole('button', { name: 'Sign in again' }))
    fireEvent.click(page.getByRole('tab', { name: 'Sign in with browser' }))
    fireEvent.click(page.getByRole('button', { name: 'Start browser sign-in' }))
    await page.findByRole('link', { name: 'Open sign-in page' })
    // From another tab, so nothing but the cancellation itself can notice
    // how the sign-in ended.
    fireEvent.click(page.getByRole('tab', { name: 'Sign in with password' }))

    fireEvent.click(page.getByRole('button', { name: 'Cancel reactivation' }))

    // Not cancelled: the account is signed in, so there is nothing left to
    // reactivate.
    await page.findByRole('heading', { name: 'Add account' })
    expect(page.services.matrixOAuthBrowser.flow.value?.stage).toBe('done')
    await page.findByRole('button', { name: 'Log out' })
    // The browser tab, where the new session is shown still to be verified.
    expect(
      page
        .getByRole('tab', { name: 'Sign in with browser' })
        .getAttribute('aria-selected'),
    ).toBe('true')
    await page.findByText(/not verified yet/)
  })

  it('finishes a reactivation whose sign-in completed while the user switched tabs', async () => {
    let state = 'deactivated'
    let releaseRefresh: () => void = () => {}
    const refreshHeld = new Promise<void>((resolve) => {
      releaseRefresh = resolve
    })
    const bob = () => [{ ...BOB, state, device_id: 'AXONDEV', verified: false }]
    server.use(
      http.post(CREATE_URL, () =>
        HttpResponse.json(
          { data: { ...WAITING.data, expected_user_id: BOB.user_id } },
          { status: 201 },
        ),
      ),
      http.get(FLOW_URL, () => {
        state = 'active'
        return HttpResponse.json({
          data: {
            flow_id: FLOW_ID,
            expected_user_id: BOB.user_id,
            stage: 'done',
            account_id: BOB.account_id,
          },
        })
      }),
    )
    const page = renderPage(bob)
    // The refresh that follows the sign-in is held, so the tab switch lands
    // while the store is still completing it.
    server.use(
      http.get(`${TEST_BASE_URL}/v1/accounts`, async () => {
        if (state === 'active') {
          await refreshHeld
        }
        return HttpResponse.json({ data: bob() })
      }),
    )
    fireEvent.click(await page.findByRole('button', { name: 'Sign in again' }))
    fireEvent.click(page.getByRole('tab', { name: 'Sign in with browser' }))
    fireEvent.click(page.getByRole('button', { name: 'Start browser sign-in' }))
    await page.findByRole('link', { name: 'Open sign-in page' })
    document.dispatchEvent(new Event('visibilitychange'))
    await waitFor(() =>
      expect(page.services.matrixOAuthBrowser.operation.value).toBe(
        'refreshing_accounts',
      ),
    )

    fireEvent.click(page.getByRole('tab', { name: 'Sign in with password' }))
    releaseRefresh()
    await waitFor(() =>
      expect(page.services.matrixOAuthBrowser.operation.value).toBe('idle'),
    )
    fireEvent.click(page.getByRole('tab', { name: 'Sign in with browser' }))

    // The sign-in is not lost: back on its tab it ends the reactivation and
    // still leads on to verification.
    await page.findByText(/not verified yet/)
    await page.findByRole('heading', { name: 'Add account' })
  })

  it('shows why a reactivation could not be cancelled, from any tab', async () => {
    const QR_FLOW = {
      data: {
        flow_id: '10000000-0000-4000-8000-000000000002',
        expected_user_id: BOB.user_id,
        presentation: 'display',
        stage: 'waiting_for_authorization',
      },
    }
    let qrCancelled = false
    server.use(
      http.post(CREATE_URL, () =>
        HttpResponse.json(
          { data: { ...WAITING.data, expected_user_id: BOB.user_id } },
          { status: 201 },
        ),
      ),
      // Approved meanwhile: Axon accepts the cancellation and changes nothing.
      http.delete(FLOW_URL, () => new HttpResponse(null, { status: 204 })),
      http.get(FLOW_URL, () =>
        HttpResponse.json({
          data: {
            flow_id: FLOW_ID,
            expected_user_id: BOB.user_id,
            stage: 'finalizing',
          },
        }),
      ),
      http.post(`${TEST_BASE_URL}/v1/accounts/login/qr`, () =>
        HttpResponse.json(QR_FLOW, { status: 201 }),
      ),
      http.get(`${TEST_BASE_URL}/v1/accounts/login/qr/:flowId`, () =>
        HttpResponse.json(QR_FLOW),
      ),
      http.delete(`${TEST_BASE_URL}/v1/accounts/login/qr/:flowId`, () => {
        qrCancelled = true
        return new HttpResponse(null, { status: 204 })
      }),
    )
    const page = renderPage([BOB])
    fireEvent.click(await page.findByRole('button', { name: 'Sign in again' }))
    fireEvent.click(page.getByRole('tab', { name: 'Sign in with browser' }))
    fireEvent.click(page.getByRole('button', { name: 'Start browser sign-in' }))
    await page.findByRole('link', { name: 'Open sign-in page' })
    fireEvent.click(page.getByRole('tab', { name: 'Sign in with QR code' }))
    fireEvent.click(page.getByRole('button', { name: 'Start QR sign-in' }))
    await page.findByText(/Approve the Matrix device authorization request/)
    fireEvent.click(page.getByRole('tab', { name: 'Sign in with password' }))

    fireEvent.click(page.getByRole('button', { name: 'Cancel reactivation' }))

    // Not a dead button: the browser tab opens on the reason.
    await page.findByText(/Too late to cancel/)
    expect(
      page
        .getByRole('tab', { name: 'Sign in with browser' })
        .getAttribute('aria-selected'),
    ).toBe('true')
    expect(
      page.getByRole('heading', { name: 'Reactivate @bob:example.org' }),
    ).toBeTruthy()
    // The QR sign-in it did cancel is gone, not left to come back.
    expect(qrCancelled).toBe(true)
    expect(page.services.matrixOAuthQr.flow.value).toBeNull()
    page.services.matrixOAuthBrowser.reset()
  })

  it('does not replay a finished sign-in when the panel is opened again', async () => {
    server.use(
      http.post(CREATE_URL, () =>
        HttpResponse.json(flow('done', { account_id: ALICE.account_id }), {
          status: 201,
        }),
      ),
    )
    const page = renderPage([ALICE])
    await page.findByText(ALICE.user_id)
    await start(page)
    await page.findByText(/not verified yet/)

    fireEvent.click(page.getByRole('tab', { name: 'Sign in with password' }))
    fireEvent.click(page.getByRole('tab', { name: 'Sign in with browser' }))

    await page.findByRole('button', { name: 'Start browser sign-in' })
    expect(page.queryByText('Signed in.')).toBeNull()
    expect(page.services.matrixOAuthBrowser.flow.value).toBeNull()
  })

  it('forgets a finished sign-in once its account is signed out', async () => {
    let state = 'active'
    server.use(
      http.post(CREATE_URL, () =>
        HttpResponse.json(flow('done', { account_id: ALICE.account_id }), {
          status: 201,
        }),
      ),
    )
    const page = renderPage(() => [{ ...ALICE, state }])
    await page.findByText(ALICE.user_id)
    await start(page)
    await page.findByText(/not verified yet/)

    state = 'deactivated'
    await page.services.accounts.refresh()

    await page.findByRole('button', { name: 'Start browser sign-in' })
    expect(page.queryByText(/not verified yet/)).toBeNull()
  })

  // The store outlives the panel and keeps polling. A sign-in that ended
  // while another tab was showing is believed only if its account is active.
  it.each([
    ['active', true],
    ['deactivated', false],
  ])(
    'treats a sign-in that finished out of sight, account now %s, by the account list',
    async (state, shown) => {
      let stage = 'waiting_for_authorization'
      server.use(
        http.post(CREATE_URL, () =>
          HttpResponse.json(WAITING, { status: 201 }),
        ),
        http.get(FLOW_URL, () =>
          HttpResponse.json(
            stage === 'done'
              ? flow('done', { account_id: ALICE.account_id })
              : WAITING,
          ),
        ),
      )
      const page = renderPage(() => [
        { ...ALICE, state: stage === 'done' ? state : 'deactivated' },
      ])
      await page.findByText(ALICE.user_id)
      await start(page)
      await page.findByRole('link', { name: 'Open sign-in page' })

      fireEvent.click(page.getByRole('tab', { name: 'Sign in with password' }))
      stage = 'done'
      page.services.matrixOAuthBrowser.checkNow()
      await waitFor(() =>
        expect(page.services.matrixOAuthBrowser.flow.value?.stage).toBe('done'),
      )
      await waitFor(() =>
        expect(page.services.matrixOAuthBrowser.operation.value).toBe('idle'),
      )

      if (shown) {
        fireEvent.click(page.getByRole('tab', { name: 'Sign in with browser' }))
        await page.findByText(/not verified yet/)
        return
      }
      // Signed out again since: reactivating must start a new sign-in, not
      // be ended at once by the old one.
      fireEvent.click(
        await page.findByRole('button', { name: 'Sign in again' }),
      )
      fireEvent.click(page.getByRole('tab', { name: 'Sign in with browser' }))
      await page.findByRole('button', { name: 'Start browser sign-in' })
      expect(page.queryByText('Signed in.')).toBeNull()
      page.getByRole('heading', { name: `Reactivate ${ALICE.user_id}` })
    },
  )
})
