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
import type { components } from '../api/schema'
import { ServicesContext } from '../services'
import { memoryStorage } from '../test/memory-storage'
import { TEST_BASE_URL, testServices } from '../test/services'
import { LinkedSignIns, RESUME_KEY } from './LinkedSignIns'

type Identity = components['schemas']['OauthIdentityDto']

const IDENTITIES_URL = `${TEST_BASE_URL}/v1/management/oauth/identities`
const APPLE = '11111111-0000-4000-8000-000000000001'
const GOOGLE = '11111111-0000-4000-8000-000000000002'

const apple: Identity = {
  id: APPLE,
  provider: 'apple',
  email: null,
  linked_at: '2026-09-01T12:00:00Z',
  current: false,
  sign_in_available: true,
}
const google: Identity = {
  id: GOOGLE,
  provider: 'google',
  email: 'owner@example.org',
  linked_at: '2026-08-01T12:00:00Z',
  current: true,
  sign_in_available: true,
}

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  window.sessionStorage.clear()
})
afterAll(() => server.close())

/** How a server answers the list when it does not serve management. */
function managementOff(status = 403) {
  return http.get(IDENTITIES_URL, () =>
    refuse(status, status === 403 ? 'management_disabled' : 'not_found'),
  )
}

function noProviders() {
  return http.get(`${TEST_BASE_URL}/v1/oauth/providers`, () =>
    HttpResponse.json({ data: [] }),
  )
}

function providers(browser: string[], native: string[] = []) {
  return http.get(`${TEST_BASE_URL}/v1/oauth/providers`, ({ request }) => {
    const names =
      new URL(request.url).searchParams.get('flow') === 'native'
        ? native
        : browser
    return HttpResponse.json({ data: names.map((provider) => ({ provider })) })
  })
}

/** A list that reflects unlinks, and a record of every DELETE it was sent. */
function identityRoutes(
  initial: Identity[],
  respond: (call: number, request: Request) => Response | null = () => null,
) {
  let listed = [...initial]
  const deletes: { id: string; allowLockout: string | null; bearer: string }[] =
    []
  const handlers = [
    http.get(IDENTITIES_URL, () => HttpResponse.json({ data: listed })),
    http.delete(`${IDENTITIES_URL}/:id`, ({ request, params }) => {
      deletes.push({
        id: String(params.id),
        allowLockout: new URL(request.url).searchParams.get('allow_lockout'),
        bearer: request.headers.get('authorization') ?? '',
      })
      const refusal = respond(deletes.length, request)
      if (refusal !== null) {
        return refusal
      }
      listed = listed.filter(({ id }) => id !== params.id)
      return new HttpResponse(null, { status: 204 })
    }),
  ]
  return { handlers, deletes }
}

function refuse(status: number, code: string) {
  return HttpResponse.json({ error: { code, message: code } }, { status })
}

function oauthSession(provider: string) {
  return memoryStorage({
    'axon.oauth.session': JSON.stringify({
      accessToken: 'old-access',
      refreshToken: 'old-refresh',
      expiresAt: Date.now() + 3_600_000,
      provider,
    }),
  })
}

function renderPanel(services = testServices()) {
  const view = render(
    <ServicesContext.Provider value={services}>
      <LinkedSignIns />
    </ServicesContext.Provider>,
  )
  return { ...view, services }
}

describe('LinkedSignIns', () => {
  it('lists nothing where management is switched off', async () => {
    server.use(managementOff(), noProviders())
    const view = renderPanel()

    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(view.container.textContent).toBe('')
  })

  it('stays hidden on a server older than the management API', async () => {
    server.use(managementOff(404), noProviders())
    const view = renderPanel()

    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(view.container.textContent).toBe('')
  })

  it('never asks for server status, which is expensive to compute', async () => {
    // `onUnhandledRequest: 'error'` fails any request with no handler, and
    // there is none here for `/v1/status`.
    const routes = identityRoutes([apple])
    server.use(noProviders(), ...routes.handlers)
    const view = renderPanel()

    await view.findByRole('button', { name: 'Unlink Apple' })
  })

  it('drops a sign-in unlinked elsewhere when the app returns to the front', async () => {
    let listed: Identity[] = [google, apple]
    server.use(
      noProviders(),
      http.get(IDENTITIES_URL, () => HttpResponse.json({ data: listed })),
    )
    const view = renderPanel()
    await view.findByRole('button', { name: 'Unlink Apple' })

    listed = [google]
    // Inside the wait: the listener is attached by an effect that may not
    // have run yet on the render the button first appeared in.
    await waitFor(() => {
      window.dispatchEvent(new Event('focus'))
      expect(view.queryByRole('button', { name: 'Unlink Apple' })).toBeNull()
    })
  })

  it('says so when the list cannot be loaded', async () => {
    server.use(
      noProviders(),
      http.get(IDENTITIES_URL, () => refuse(500, 'internal')),
    )
    const view = renderPanel()

    await view.findByText('Could not load linked sign-ins: internal')
  })

  it('lists each linked sign-in, marking the current and the unusable', async () => {
    const routes = identityRoutes([
      google,
      { ...apple, sign_in_available: false },
    ])
    server.use(noProviders(), ...routes.handlers)
    const view = renderPanel()

    await view.findByRole('heading', { name: 'Linked sign-ins' })
    const rows = view.container.querySelectorAll('.linked-sign-ins > li')
    expect(rows).toHaveLength(2)
    expect(rows[0].textContent).toContain('Google · owner@example.org')
    expect(rows[0].textContent).toContain('This session')
    expect(rows[1].textContent).toContain('Apple')
    expect(rows[1].textContent).toContain('switched off on this server')
    expect(rows[1].textContent).not.toContain('This session')
  })

  it('unlinks after a confirmation that points at the provider', async () => {
    const routes = identityRoutes([google, apple])
    server.use(noProviders(), ...routes.handlers)
    const view = renderPanel()

    fireEvent.click(await view.findByRole('button', { name: 'Unlink Apple' }))
    // Nothing is sent until the consequence has been read.
    expect(routes.deletes).toHaveLength(0)
    // Says what it does, then what it leaves alone at the provider.
    expect(
      view.getByText(/this Apple account can no longer sign in/),
    ).toBeTruthy()
    expect(view.getByText(/This only changes Axon/)).toBeTruthy()
    expect(
      view.getByText(/choose your name, then Sign in with Apple\./),
    ).toBeTruthy()

    fireEvent.click(view.getByRole('button', { name: 'Unlink Apple' }))

    await waitFor(() =>
      expect(
        view.container.querySelectorAll('.linked-sign-ins > li'),
      ).toHaveLength(1),
    )
    expect(routes.deletes).toEqual([
      { id: APPLE, allowLockout: null, bearer: 'Bearer tok-test' },
    ])
    expect(view.services.auth.signedIn.value).toBe(true)
  })

  it('cancels without sending anything', async () => {
    const routes = identityRoutes([apple])
    server.use(noProviders(), ...routes.handlers)
    const view = renderPanel()

    fireEvent.click(await view.findByRole('button', { name: 'Unlink Apple' }))
    fireEvent.click(view.getByRole('button', { name: 'Cancel' }))

    expect(view.queryByText(/This only changes Axon/)).toBeNull()
    expect(routes.deletes).toHaveLength(0)
  })

  it('explains a last credential and only then repeats with allow_lockout', async () => {
    const routes = identityRoutes([apple], (call) =>
      call === 1 ? refuse(409, 'last_credential') : null,
    )
    server.use(noProviders(), ...routes.handlers)
    const view = renderPanel()

    fireEvent.click(await view.findByRole('button', { name: 'Unlink Apple' }))
    fireEvent.click(view.getByRole('button', { name: 'Unlink Apple' }))

    await view.findByText(/This is the last way to sign in/)
    expect(routes.deletes).toHaveLength(1)
    expect(routes.deletes[0].allowLockout).toBeNull()

    fireEvent.click(await view.findByRole('button', { name: 'Unlink anyway' }))

    await waitFor(() => expect(routes.deletes).toHaveLength(2))
    expect(routes.deletes[1].allowLockout).toBe('true')
    // Nothing left to list and nothing to link: the panel goes with it.
    await waitFor(() => expect(view.container.textContent).toBe(''))
  })

  it('signs out after unlinking the identity this session came from', async () => {
    const routes = identityRoutes([google])
    server.use(noProviders(), ...routes.handlers)
    const view = renderPanel()

    fireEvent.click(
      await view.findByRole('button', {
        name: 'Unlink Google owner@example.org',
      }),
    )
    expect(view.getByText(/including this one/)).toBeTruthy()
    fireEvent.click(view.getByRole('button', { name: 'Unlink Google' }))

    await waitFor(() => expect(view.services.auth.signedIn.value).toBe(false))
  })

  it('reports a failure and leaves the list as it was', async () => {
    const routes = identityRoutes([apple], () => refuse(500, 'internal'))
    server.use(noProviders(), ...routes.handlers)
    const view = renderPanel()

    fireEvent.click(await view.findByRole('button', { name: 'Unlink Apple' }))
    fireEvent.click(view.getByRole('button', { name: 'Unlink Apple' }))

    expect((await view.findByRole('alert')).textContent).toBe('internal')
    expect(
      view.container.querySelectorAll('.linked-sign-ins > li'),
    ).toHaveLength(1)
  })
})

describe('a change the server wants a fresh sign-in for', () => {
  it('re-runs a native sign-in in place and retries with the new session', async () => {
    const routes = identityRoutes(
      [
        { ...apple, current: true },
        { ...google, current: false },
      ],
      (call) => (call === 1 ? refuse(403, 'recent_sign_in_required') : null),
    )
    server.use(
      providers([], ['apple']),
      ...routes.handlers,
      http.post(`${TEST_BASE_URL}/v1/oauth/apple/native/challenge`, () =>
        HttpResponse.json({ challenge: 'c', nonce: 'n', expires_in: 300 }),
      ),
      http.post(`${TEST_BASE_URL}/v1/oauth/apple/native/token`, () =>
        HttpResponse.json({
          access_token: 'fresh-access',
          token_type: 'Bearer',
          expires_in: 3600,
          refresh_token: 'fresh-refresh',
        }),
      ),
    )
    const appleSignIn = vi.fn(() => Promise.resolve('identity-token'))
    const view = renderPanel(
      testServices({
        token: null,
        storage: oauthSession('apple'),
        platform: { appleSignIn },
      }),
    )

    fireEvent.click(
      await view.findByRole('button', {
        name: 'Unlink Google owner@example.org',
      }),
    )
    fireEvent.click(view.getByRole('button', { name: 'Unlink Google' }))

    await waitFor(() => expect(routes.deletes).toHaveLength(2))
    expect(appleSignIn).toHaveBeenCalledTimes(1)
    expect(routes.deletes.map(({ bearer }) => bearer)).toEqual([
      'Bearer old-access',
      'Bearer fresh-access',
    ])
    await waitFor(() =>
      expect(
        view.container.querySelectorAll('.linked-sign-ins > li'),
      ).toHaveLength(1),
    )
    expect(window.sessionStorage.getItem(RESUME_KEY)).toBeNull()
  })

  it('does not ask for a second sign-in when the first was not enough', async () => {
    const routes = identityRoutes([apple], () =>
      refuse(403, 'recent_sign_in_required'),
    )
    server.use(
      providers([], ['apple']),
      ...routes.handlers,
      http.post(`${TEST_BASE_URL}/v1/oauth/apple/native/challenge`, () =>
        HttpResponse.json({ challenge: 'c', nonce: 'n', expires_in: 300 }),
      ),
      http.post(`${TEST_BASE_URL}/v1/oauth/apple/native/token`, () =>
        HttpResponse.json({
          access_token: 'fresh-access',
          token_type: 'Bearer',
          expires_in: 3600,
          refresh_token: 'fresh-refresh',
        }),
      ),
    )
    const appleSignIn = vi.fn(() => Promise.resolve('identity-token'))
    const view = renderPanel(
      testServices({
        token: null,
        storage: oauthSession('apple'),
        platform: { appleSignIn },
      }),
    )

    fireEvent.click(await view.findByRole('button', { name: 'Unlink Apple' }))
    fireEvent.click(view.getByRole('button', { name: 'Unlink Apple' }))

    expect((await view.findByRole('alert')).textContent).toMatch(
      /still needs a more recent sign-in/,
    )
    expect(appleSignIn).toHaveBeenCalledTimes(1)
    expect(routes.deletes).toHaveLength(2)
  })

  it('hands a browser sign-in off and finishes when it comes back', async () => {
    const routes = identityRoutes([google, apple], (call) =>
      call === 1 ? refuse(403, 'recent_sign_in_required') : null,
    )
    server.use(
      providers(['google']),
      ...routes.handlers,
      http.post(`${TEST_BASE_URL}/v1/oauth/token`, () =>
        HttpResponse.json({
          access_token: 'fresh-access',
          token_type: 'Bearer',
          expires_in: 3600,
          refresh_token: 'fresh-refresh',
        }),
      ),
    )
    const navigate = vi.fn()
    const pendingStorage = memoryStorage()
    const view = renderPanel(
      testServices({
        token: null,
        storage: oauthSession('google'),
        pendingStorage,
        navigate,
      }),
    )

    fireEvent.click(await view.findByRole('button', { name: 'Unlink Apple' }))
    fireEvent.click(view.getByRole('button', { name: 'Unlink Apple' }))

    await view.findByText(/Continue signing in in your browser/)
    const authorize = new URL(String(navigate.mock.calls[0][0]))
    expect(authorize.searchParams.get('provider')).toBe('google')
    const pending = JSON.parse(
      pendingStorage.getItem('axon.oauth.pending') ?? '{}',
    ) as { state: string; returnTo?: string }
    expect(pending.returnTo).toBe('/settings')
    expect(routes.deletes).toHaveLength(1)

    // The sign-in comes back; in a shell, as a deep link into this same page.
    const result = await view.services.auth.completeOAuthRedirect(
      new URL(
        `org.matrixaxon.axon:/oauth/callback?code=c&state=${pending.state}`,
      ),
    )
    expect(result).toEqual({ ok: true })

    await waitFor(() => expect(routes.deletes).toHaveLength(2))
    expect(routes.deletes[1]).toEqual({
      id: APPLE,
      allowLockout: null,
      bearer: 'Bearer fresh-access',
    })
    expect(window.sessionStorage.getItem(RESUME_KEY)).toBeNull()
    await waitFor(() =>
      expect(view.queryByText(/Continue signing in/)).toBeNull(),
    )
  })

  it('leaves a waiting change alone when no sign-in followed it', async () => {
    const routes = identityRoutes([apple])
    server.use(noProviders(), ...routes.handlers)
    window.sessionStorage.setItem(
      RESUME_KEY,
      JSON.stringify({
        kind: 'unlink',
        identityId: APPLE,
        allowLockout: false,
        current: false,
        createdAt: Date.now(),
      }),
    )
    const view = renderPanel()

    await view.findByRole('button', { name: 'Unlink Apple' })
    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(routes.deletes).toHaveLength(0)
  })

  it('does not finish a waiting unlink on a sign-in made for something else', async () => {
    // The unlink was refused, the user backed out of the provider's page, and
    // then linked an Apple ID instead. That sign-in must not complete the
    // unlink they walked away from.
    const routes = identityRoutes([{ ...google, current: false }, apple])
    server.use(
      providers([], ['apple']),
      ...routes.handlers,
      http.post(`${TEST_BASE_URL}/v1/oauth/apple/native/challenge`, () =>
        HttpResponse.json({ challenge: 'c', nonce: 'n', expires_in: 300 }),
      ),
      http.post(`${TEST_BASE_URL}/v1/oauth/apple/native/token`, () =>
        HttpResponse.json({
          access_token: 'apple-access',
          token_type: 'Bearer',
          expires_in: 3600,
          refresh_token: 'apple-refresh',
        }),
      ),
    )
    window.sessionStorage.setItem(
      RESUME_KEY,
      JSON.stringify({
        kind: 'unlink',
        identityId: GOOGLE,
        allowLockout: false,
        current: false,
        createdAt: Date.now() - 1000,
      }),
    )
    const view = renderPanel(
      testServices({
        platform: { appleSignIn: () => Promise.resolve('identity-token') },
      }),
    )

    fireEvent.click(
      await view.findByRole('button', { name: 'Link an Apple ID' }),
    )

    await waitFor(() =>
      expect(view.services.auth.oauth.sessionProvider.value).toBe('apple'),
    )
    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(routes.deletes).toHaveLength(0)
    expect(window.sessionStorage.getItem(RESUME_KEY)).toBeNull()
  })

  it('withdraws a waiting unlink on Cancel', async () => {
    const routes = identityRoutes([apple])
    server.use(noProviders(), ...routes.handlers)
    const view = renderPanel()
    fireEvent.click(await view.findByRole('button', { name: 'Unlink Apple' }))
    window.sessionStorage.setItem(
      RESUME_KEY,
      JSON.stringify({
        kind: 'unlink',
        identityId: APPLE,
        allowLockout: false,
        current: false,
        createdAt: Date.now(),
      }),
    )

    fireEvent.click(view.getByRole('button', { name: 'Cancel' }))

    expect(window.sessionStorage.getItem(RESUME_KEY)).toBeNull()
  })

  it('says what to do when there is no sign-in to re-run', async () => {
    // A pasted token that expires: not an OAuth session this client holds.
    const routes = identityRoutes([apple], () =>
      refuse(403, 'recent_sign_in_required'),
    )
    server.use(noProviders(), ...routes.handlers)
    const view = renderPanel()

    fireEvent.click(await view.findByRole('button', { name: 'Unlink Apple' }))
    fireEvent.click(view.getByRole('button', { name: 'Unlink Apple' }))

    expect((await view.findByRole('alert')).textContent).toMatch(
      /needs a sign-in from the last ten minutes/,
    )
    expect(window.sessionStorage.getItem(RESUME_KEY)).toBeNull()
  })
})

describe('linking an Apple ID', () => {
  function nativeRoutes(challenges: (string | null)[]) {
    return [
      http.post(
        `${TEST_BASE_URL}/v1/oauth/apple/native/challenge`,
        ({ request }) => {
          challenges.push(request.headers.get('authorization'))
          return HttpResponse.json({
            challenge: 'c',
            nonce: 'n',
            expires_in: 300,
          })
        },
      ),
      http.post(`${TEST_BASE_URL}/v1/oauth/apple/native/token`, () =>
        HttpResponse.json({
          access_token: 'apple-access',
          token_type: 'Bearer',
          expires_in: 3600,
          refresh_token: 'apple-refresh',
        }),
      ),
    ]
  }

  it('links with the current bearer and shows the new identity', async () => {
    const challenges: (string | null)[] = []
    let listed: Identity[] = [google]
    server.use(
      providers([], ['apple']),
      http.get(IDENTITIES_URL, () => HttpResponse.json({ data: listed })),
      ...nativeRoutes(challenges),
    )
    const appleSignIn = vi.fn(() => {
      listed = [
        { ...apple, current: true },
        { ...google, current: false },
      ]
      return Promise.resolve('identity-token')
    })
    const view = renderPanel(testServices({ platform: { appleSignIn } }))

    fireEvent.click(
      await view.findByRole('button', { name: 'Link an Apple ID' }),
    )

    await view.findByRole('button', { name: 'Unlink Apple' })
    expect(challenges).toEqual(['Bearer tok-test'])
    expect(view.services.auth.oauth.sessionProvider.value).toBe('apple')
  })

  it('still offers linking where management is switched off', async () => {
    const challenges: (string | null)[] = []
    server.use(
      managementOff(),
      providers([], ['apple']),
      ...nativeRoutes(challenges),
    )
    const view = renderPanel(
      testServices({
        platform: { appleSignIn: () => Promise.resolve('identity-token') },
      }),
    )

    fireEvent.click(
      await view.findByRole('button', { name: 'Link an Apple ID' }),
    )

    // Linking adopts the Apple session, which is itself the proof of the link.
    await view.findByText(/You are signed in with Apple/)
    expect(view.queryByRole('button')).toBeNull()
    expect(view.queryByRole('heading', { name: 'Linked sign-ins' })).toBeNull()
  })

  it('offers nothing where native Apple is not available', async () => {
    server.use(managementOff(), noProviders())
    const view = renderPanel(
      testServices({ platform: { appleSignIn: vi.fn() } }),
    )

    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(view.container.textContent).toBe('')
  })
})
