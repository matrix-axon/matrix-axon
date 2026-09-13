import { fireEvent, render, waitFor } from '@testing-library/preact'
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
import { memoryStorage } from '../test/memory-storage'
import { createOAuthAuthProvider, parseOAuthProviders } from './oauth'

const BASE_URL = 'http://axon.test'
const TOKEN_URL = `${BASE_URL}/v1/oauth/token`

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  server.resetHandlers()
  history.replaceState(null, '', '/')
})
afterAll(() => server.close())

describe('parseOAuthProviders', () => {
  it('parses deployment configured provider labels', () => {
    expect(parseOAuthProviders('google:Google,microsoft:Microsoft')).toEqual([
      { provider: 'google', label: 'Google' },
      { provider: 'microsoft', label: 'Microsoft' },
    ])
  })

  it('defaults missing labels and rejects invalid provider ids', () => {
    expect(parseOAuthProviders('google, bad id:Bad,ms-work:Work')).toEqual([
      { provider: 'google', label: 'google' },
      { provider: 'ms-work', label: 'Work' },
    ])
  })
})

describe('createOAuthAuthProvider', () => {
  it('redeems a callback code and stores the returned token pair', async () => {
    const storage = memoryStorage()
    const pending = memoryStorage({
      'axon.oauth.pending': JSON.stringify({
        state: 'state-123',
        codeVerifier: 'verifier-123',
        provider: 'google',
        redirectUri: 'http://localhost:3000/oauth/callback',
        createdAt: Date.now(),
      }),
    })
    let form = ''
    server.use(
      http.post(TOKEN_URL, async ({ request }) => {
        form = await request.text()
        return HttpResponse.json({
          access_token: 'access-1',
          token_type: 'Bearer',
          expires_in: 3600,
          refresh_token: 'refresh-1',
        })
      }),
    )

    const auth = createOAuthAuthProvider({
      providers: [{ provider: 'google', label: 'Google' }],
      baseUrl: BASE_URL,
      storage,
      pendingStorage: pending,
    })
    const result = await auth.completeRedirect(
      new URL(
        'http://localhost:3000/oauth/callback?code=code-1&state=state-123',
      ),
    )

    expect(result).toEqual({ ok: true })
    expect(auth.signedIn.value).toBe(true)
    expect(auth.getToken()).toBe('access-1')
    expect(pending.getItem('axon.oauth.pending')).toBeNull()
    expect(new URLSearchParams(form).get('grant_type')).toBe(
      'authorization_code',
    )
    expect(new URLSearchParams(form).get('code_verifier')).toBe('verifier-123')
  })

  it('stores a one-time OAuth session in session storage', async () => {
    const storage = memoryStorage()
    const sessionStorage = memoryStorage()
    const pending = memoryStorage({
      'axon.oauth.pending': JSON.stringify({
        state: 'state-123',
        codeVerifier: 'verifier-123',
        provider: 'google',
        redirectUri: 'http://localhost:3000/oauth/callback',
        createdAt: Date.now(),
        storageMode: 'session',
      }),
    })
    server.use(
      http.post(TOKEN_URL, () =>
        HttpResponse.json({
          access_token: 'access-1',
          token_type: 'Bearer',
          expires_in: 3600,
          refresh_token: 'refresh-1',
        }),
      ),
    )

    const auth = createOAuthAuthProvider({
      providers: [{ provider: 'google', label: 'Google' }],
      baseUrl: BASE_URL,
      storage,
      sessionStorage,
      pendingStorage: pending,
    })
    const result = await auth.completeRedirect(
      new URL(
        'http://localhost:3000/oauth/callback?code=code-1&state=state-123',
      ),
    )

    expect(result).toEqual({ ok: true })
    expect(storage.getItem('axon.oauth.session')).toBeNull()
    expect(sessionStorage.getItem('axon.oauth.session')).not.toBeNull()
  })

  it('rejects a callback with mismatched state without calling token exchange', async () => {
    const pending = memoryStorage({
      'axon.oauth.pending': JSON.stringify({
        state: 'expected',
        codeVerifier: 'verifier-123',
        provider: 'google',
        redirectUri: 'http://localhost:3000/oauth/callback',
        createdAt: Date.now(),
      }),
    })
    const auth = createOAuthAuthProvider({
      providers: [{ provider: 'google', label: 'Google' }],
      baseUrl: BASE_URL,
      storage: memoryStorage(),
      pendingStorage: pending,
    })

    const result = await auth.completeRedirect(
      new URL('http://localhost:3000/oauth/callback?code=code-1&state=wrong'),
    )

    expect(result).toEqual({
      ok: false,
      message: 'OAuth sign-in state did not match',
    })
    expect(auth.signedIn.value).toBe(false)
    expect(pending.getItem('axon.oauth.pending')).toBeNull()
  })

  it('refreshes an expired access token and rotates the stored refresh token', async () => {
    const storage = memoryStorage({
      'axon.oauth.session': JSON.stringify({
        accessToken: 'old-access',
        refreshToken: 'old-refresh',
        expiresAt: Date.now() - 1000,
        provider: 'microsoft',
      }),
    })
    let form = ''
    server.use(
      http.post(TOKEN_URL, async ({ request }) => {
        form = await request.text()
        return HttpResponse.json({
          access_token: 'new-access',
          token_type: 'Bearer',
          expires_in: 3600,
          refresh_token: 'new-refresh',
        })
      }),
    )
    const auth = createOAuthAuthProvider({
      providers: [{ provider: 'microsoft', label: 'Microsoft' }],
      baseUrl: BASE_URL,
      storage,
      pendingStorage: memoryStorage(),
    })

    await expect(auth.getToken()).resolves.toBe('new-access')

    const saved = JSON.parse(storage.getItem('axon.oauth.session')!)
    expect(new URLSearchParams(form).get('grant_type')).toBe('refresh_token')
    expect(new URLSearchParams(form).get('refresh_token')).toBe('old-refresh')
    expect(saved.accessToken).toBe('new-access')
    expect(saved.refreshToken).toBe('new-refresh')
    expect(saved.provider).toBe('microsoft')
  })

  it('refreshes an expired one-time access token without promoting it', async () => {
    const storage = memoryStorage()
    const sessionStorage = memoryStorage({
      'axon.oauth.session': JSON.stringify({
        accessToken: 'old-access',
        refreshToken: 'old-refresh',
        expiresAt: Date.now() - 1000,
        provider: 'microsoft',
      }),
    })
    server.use(
      http.post(TOKEN_URL, () =>
        HttpResponse.json({
          access_token: 'new-access',
          token_type: 'Bearer',
          expires_in: 3600,
          refresh_token: 'new-refresh',
        }),
      ),
    )
    const auth = createOAuthAuthProvider({
      providers: [{ provider: 'microsoft', label: 'Microsoft' }],
      baseUrl: BASE_URL,
      storage,
      sessionStorage,
      pendingStorage: memoryStorage(),
    })

    await expect(auth.getToken()).resolves.toBe('new-access')

    expect(storage.getItem('axon.oauth.session')).toBeNull()
    const saved = JSON.parse(sessionStorage.getItem('axon.oauth.session')!)
    expect(saved.accessToken).toBe('new-access')
    expect(saved.refreshToken).toBe('new-refresh')
  })

  it('clears the OAuth session when refresh fails', async () => {
    const storage = memoryStorage({
      'axon.oauth.session': JSON.stringify({
        accessToken: 'old-access',
        refreshToken: 'old-refresh',
        expiresAt: Date.now() - 1000,
        provider: 'google',
      }),
    })
    server.use(
      http.post(TOKEN_URL, () =>
        HttpResponse.json(
          {
            error: 'invalid_grant',
            error_description: 'refresh token expired',
          },
          { status: 400 },
        ),
      ),
    )
    const auth = createOAuthAuthProvider({
      providers: [{ provider: 'google', label: 'Google' }],
      baseUrl: BASE_URL,
      storage,
      pendingStorage: memoryStorage(),
    })

    await expect(auth.getToken()).resolves.toBeNull()
    expect(auth.signedIn.value).toBe(false)
    expect(storage.getItem('axon.oauth.session')).toBeNull()
  })

  /**
   * A refresh that never got an answer must not end the session. This is the
   * deploy-time sign-out: restarting the server drops the live socket, the
   * reconnect asks for a token, the hour-old access token needs refreshing, and
   * that refresh lands on the process still coming back up. Discarding a
   * 30-day refresh token over a connection refused is what forced testers to
   * sign in again after every push.
   */
  describe('a refresh that gets no verdict keeps the session', () => {
    const expiredSession = () =>
      memoryStorage({
        'axon.oauth.session': JSON.stringify({
          accessToken: 'old-access',
          refreshToken: 'old-refresh',
          expiresAt: Date.now() - 1000,
          provider: 'google',
        }),
      })

    const providerOver = (storage: Storage) =>
      createOAuthAuthProvider({
        providers: [{ provider: 'google', label: 'Google' }],
        baseUrl: BASE_URL,
        storage,
        pendingStorage: memoryStorage(),
      })

    it('keeps it when the server cannot be reached', async () => {
      const storage = expiredSession()
      server.use(http.post(TOKEN_URL, () => HttpResponse.error()))
      const auth = providerOver(storage)

      await expect(auth.getToken()).resolves.toBeNull()

      expect(auth.signedIn.value).toBe(true)
      const saved = JSON.parse(storage.getItem('axon.oauth.session')!)
      expect(saved.refreshToken).toBe('old-refresh')
    })

    it.each([500, 502, 503, 504])('keeps it on a %i', async (status) => {
      const storage = expiredSession()
      server.use(http.post(TOKEN_URL, () => new HttpResponse(null, { status })))
      const auth = providerOver(storage)

      await expect(auth.getToken()).resolves.toBeNull()
      expect(auth.signedIn.value).toBe(true)
    })

    /**
     * `/v1/oauth/token` sits behind the OAuth rate limiter (30/min per IP), and
     * a deploy is exactly when it trips: every tab's socket drops at once and
     * every reconnect asks for a token. Classifying by status class read this
     * 4xx as a refusal and discarded a valid refresh token — the same
     * deploy-time sign-out the transport/rejection split exists to prevent,
     * through a narrower door. Note the body is Axon's envelope, not an OAuth
     * error body, so `error` is an object rather than a code.
     */
    it('keeps it on a 429 from the oauth rate limiter', async () => {
      const storage = expiredSession()
      server.use(
        http.post(TOKEN_URL, () =>
          HttpResponse.json(
            {
              error: {
                code: 'too_many_requests',
                message: 'rate limit exceeded',
              },
            },
            { status: 429 },
          ),
        ),
      )
      const auth = providerOver(storage)

      await expect(auth.getToken()).resolves.toBeNull()
      expect(auth.signedIn.value).toBe(true)
      const saved = JSON.parse(storage.getItem('axon.oauth.session')!)
      expect(saved.refreshToken).toBe('old-refresh')
    })

    // A 4xx whose body names no OAuth error code says nothing about the grant.
    it.each([
      ['a 400 with no error code', 400, {}],
      ['a 401 with an Axon envelope', 401, { error: { code: 'unauthorized' } }],
      ['a 403', 403, { error: 'access_denied' }],
      ['a 404 (oauth disabled)', 404, { error: { code: 'not_found' } }],
    ])('keeps it on %s', async (_label, status, body) => {
      const storage = expiredSession()
      server.use(
        http.post(TOKEN_URL, () => HttpResponse.json(body, { status })),
      )
      const auth = providerOver(storage)

      await expect(auth.getToken()).resolves.toBeNull()
      expect(auth.signedIn.value).toBe(true)
    })

    // …but the one code that does mean the grant is dead still ends it.
    it('still ends the session on invalid_grant', async () => {
      const storage = expiredSession()
      server.use(
        http.post(TOKEN_URL, () =>
          HttpResponse.json({ error: 'invalid_grant' }, { status: 400 }),
        ),
      )
      const auth = providerOver(storage)

      await expect(auth.getToken()).resolves.toBeNull()
      expect(auth.signedIn.value).toBe(false)
      expect(storage.getItem('axon.oauth.session')).toBeNull()
    })

    it('recovers on the next attempt once the server is back', async () => {
      const storage = expiredSession()
      let attempts = 0
      server.use(
        http.post(TOKEN_URL, () => {
          attempts += 1
          return attempts === 1
            ? HttpResponse.error()
            : HttpResponse.json({
                access_token: 'new-access',
                token_type: 'Bearer',
                expires_in: 3600,
                refresh_token: 'new-refresh',
              })
        }),
      )
      const auth = providerOver(storage)

      await expect(auth.getToken()).resolves.toBeNull()
      // The refresh token survived the outage, so the retry just works.
      await expect(auth.getToken()).resolves.toBe('new-access')
      expect(auth.signedIn.value).toBe(true)
    })
  })

  describe('onAuthFailure', () => {
    const liveSession = () =>
      memoryStorage({
        'axon.oauth.session': JSON.stringify({
          accessToken: 'old-access',
          refreshToken: 'old-refresh',
          // Not expired: `getToken` would hand this back without refreshing,
          // so a 401 here is the unexpected kind.
          expiresAt: Date.now() + 3_600_000,
          provider: 'google',
        }),
      })

    const providerOver = (storage: Storage) =>
      createOAuthAuthProvider({
        providers: [{ provider: 'google', label: 'Google' }],
        baseUrl: BASE_URL,
        storage,
        pendingStorage: memoryStorage(),
      })

    it('refreshes instead of signing out', async () => {
      const storage = liveSession()
      let attempts = 0
      server.use(
        http.post(TOKEN_URL, () => {
          attempts += 1
          return HttpResponse.json({
            access_token: 'new-access',
            token_type: 'Bearer',
            expires_in: 3600,
            refresh_token: 'new-refresh',
          })
        }),
      )
      const auth = providerOver(storage)

      auth.onAuthFailure()

      await vi.waitFor(() => expect(attempts).toBe(1))
      expect(auth.signedIn.value).toBe(true)
      // Synchronous: the freshly minted token is nowhere near expiry, so the
      // next request carries it without another round trip.
      expect(auth.getToken()).toBe('new-access')
    })

    it('signs out when the server refuses the refresh', async () => {
      const storage = liveSession()
      server.use(
        http.post(TOKEN_URL, () =>
          HttpResponse.json({ error: 'invalid_grant' }, { status: 400 }),
        ),
      )
      const auth = providerOver(storage)

      auth.onAuthFailure()

      await vi.waitFor(() => expect(auth.signedIn.value).toBe(false))
      expect(storage.getItem('axon.oauth.session')).toBeNull()
    })

    it('keeps the session when the refresh cannot reach the server', async () => {
      const storage = liveSession()
      let attempts = 0
      server.use(
        http.post(TOKEN_URL, () => {
          attempts += 1
          return HttpResponse.error()
        }),
      )
      const auth = providerOver(storage)

      auth.onAuthFailure()

      await vi.waitFor(() => expect(attempts).toBe(1))
      expect(auth.signedIn.value).toBe(true)
    })

    // A server answering 401 to everything while still honoring refreshes
    // would otherwise mint a token per failed request.
    it('rate-limits repeated failures', async () => {
      const storage = liveSession()
      let attempts = 0
      server.use(
        http.post(TOKEN_URL, () => {
          attempts += 1
          return HttpResponse.json({
            access_token: `access-${attempts}`,
            token_type: 'Bearer',
            expires_in: 3600,
            refresh_token: 'new-refresh',
          })
        }),
      )
      const auth = providerOver(storage)

      for (let i = 0; i < 5; i += 1) {
        auth.onAuthFailure()
      }

      await vi.waitFor(() => expect(attempts).toBe(1))
      expect(attempts).toBe(1)
    })

    it('does nothing when there is no session to refresh', async () => {
      const auth = providerOver(memoryStorage())
      // No handler registered: msw is set to error on unhandled requests, so a
      // request here would fail the test outright.
      auth.onAuthFailure()
      expect(auth.signedIn.value).toBe(false)
    })
  })
})

describe('a shell callback URI', () => {
  it('is used verbatim, not composed from a base', async () => {
    // Composition mangles a custom scheme: resolving `/oauth/callback` against
    // `org.matrixaxon.axon:/oauth` gives
    // `org.matrixaxon.axon:/oauth/oauth/callback`. It is also the value
    // the server allow-lists *exactly* (`OAuthClients::redirect_uri_allowed`),
    // so a derived one would be rejected at `/v1/oauth/authorize`.
    const pending = memoryStorage()
    const navigated: string[] = []
    const auth = createOAuthAuthProvider({
      providers: [{ provider: 'google', label: 'Google' }],
      baseUrl: BASE_URL,
      clientId: 'axon-desktop',
      redirectUri: 'org.matrixaxon.axon:/oauth/callback',
      storage: memoryStorage(),
      pendingStorage: pending,
      navigate: (url) => {
        navigated.push(url)
      },
    })

    await auth.startSignIn('google')

    const authorize = new URL(navigated[0])
    expect(authorize.searchParams.get('redirect_uri')).toBe(
      'org.matrixaxon.axon:/oauth/callback',
    )
    expect(authorize.searchParams.get('client_id')).toBe('axon-desktop')
    // And it must be what the token exchange later presents back, since the
    // server checks the two match.
    const stored = JSON.parse(pending.getItem('axon.oauth.pending') ?? '{}')
    expect(stored.redirectUri).toBe('org.matrixaxon.axon:/oauth/callback')
  })

  it('still composes from the origin when no URI is given', async () => {
    const navigated: string[] = []
    const auth = createOAuthAuthProvider({
      providers: [{ provider: 'google', label: 'Google' }],
      baseUrl: BASE_URL,
      storage: memoryStorage(),
      pendingStorage: memoryStorage(),
      navigate: (url) => {
        navigated.push(url)
      },
    })

    await auth.startSignIn('google')

    expect(new URL(navigated[0]).searchParams.get('redirect_uri')).toBe(
      'http://localhost:3000/oauth/callback',
    )
  })
})

describe('discovering providers from the server', () => {
  const provider = (
    options: Partial<Parameters<typeof createOAuthAuthProvider>[0]> = {},
  ) =>
    createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      storage: memoryStorage(),
      pendingStorage: memoryStorage(),
      ...options,
    })

  it('replaces the built-in list with what the server offers', async () => {
    // The list is a property of the server. A binary pointed at someone else's
    // axon cannot have been built knowing it.
    server.use(
      http.get(`${BASE_URL}/v1/oauth/providers`, () =>
        HttpResponse.json({ data: [{ provider: 'google' }] }),
      ),
    )
    const auth = provider()
    await auth.discoverProviders()

    expect(auth.providers.value).toEqual([
      { provider: 'google', label: 'Google' },
    ])
  })

  it('keeps a configured label for a provider the server names', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/oauth/providers`, () =>
        HttpResponse.json({ data: [{ provider: 'google' }] }),
      ),
    )
    const auth = provider({
      providers: [{ provider: 'google', label: 'Work Google' }],
    })
    await auth.discoverProviders()

    expect(auth.providers.value).toEqual([
      { provider: 'google', label: 'Work Google' },
    ])
  })

  it('keeps the configured list when the server has no such route', async () => {
    // A server older than GET /v1/oauth/providers, or one with OAuth off,
    // 404s. Treating that as "no providers" would delete working sign-in
    // buttons from every existing browser deployment.
    server.use(
      http.get(`${BASE_URL}/v1/oauth/providers`, () =>
        HttpResponse.json(
          { error: { code: 'not_found', message: 'nope' } },
          { status: 404 },
        ),
      ),
    )
    const configured = [{ provider: 'google', label: 'Google' }]
    const auth = provider({ providers: configured })
    await auth.discoverProviders()

    expect(auth.providers.value).toEqual(configured)
  })

  it('keeps the configured list when the server cannot be reached', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/oauth/providers`, () => HttpResponse.error()),
    )
    const configured = [{ provider: 'google', label: 'Google' }]
    const auth = provider({ providers: configured })
    await auth.discoverProviders()

    expect(auth.providers.value).toEqual(configured)
  })

  it('reports an empty server list as empty', async () => {
    // Distinct from the failures above: OAuth is on and the server genuinely
    // has nothing configured, so the buttons should go away.
    server.use(
      http.get(`${BASE_URL}/v1/oauth/providers`, () =>
        HttpResponse.json({ data: [] }),
      ),
    )
    const auth = provider({
      providers: [{ provider: 'google', label: 'Google' }],
    })
    await auth.discoverProviders()

    expect(auth.providers.value).toEqual([])
  })

  it('asks once however many times it is called', async () => {
    let calls = 0
    server.use(
      http.get(`${BASE_URL}/v1/oauth/providers`, () => {
        calls += 1
        return HttpResponse.json({ data: [{ provider: 'apple' }] })
      }),
    )
    const auth = provider()
    await Promise.all([
      auth.discoverProviders(),
      auth.discoverProviders(),
      auth.discoverProviders(),
    ])

    expect(calls).toBe(1)
    expect(auth.providers.value).toEqual([
      { provider: 'apple', label: 'Apple' },
    ])
  })

  it('asks again after an attempt that could not answer', async () => {
    // "Could not ask" is not an answer, and caching it makes a moment's
    // unavailability permanent. A packaged build has no configured list to
    // fall back on, so this was the difference between sign-in buttons
    // appearing when the server came back and never appearing again.
    let calls = 0
    server.use(
      http.get(`${BASE_URL}/v1/oauth/providers`, () => {
        calls += 1
        return calls === 1
          ? HttpResponse.error()
          : HttpResponse.json({ data: [{ provider: 'google' }] })
      }),
    )
    const auth = provider()

    await auth.discoverProviders()
    expect(auth.providers.value).toEqual([])

    await auth.discoverProviders()
    expect(calls).toBe(2)
    expect(auth.providers.value).toEqual([
      { provider: 'google', label: 'Google' },
    ])
  })

  it('still shares one request while an unanswerable attempt is in flight', async () => {
    // Not remembering the attempt must not cost the single-flight property:
    // concurrent callers share the request, and only a *later* call re-asks.
    let calls = 0
    server.use(
      http.get(`${BASE_URL}/v1/oauth/providers`, () => {
        calls += 1
        return HttpResponse.error()
      }),
    )
    const auth = provider()
    await Promise.all([
      auth.discoverProviders(),
      auth.discoverProviders(),
      auth.discoverProviders(),
    ])

    expect(calls).toBe(1)
  })

  it('keeps a determinate answer without re-asking', async () => {
    // The counterpart: an empty list *is* an answer, and must not be retried.
    let calls = 0
    server.use(
      http.get(`${BASE_URL}/v1/oauth/providers`, () => {
        calls += 1
        return HttpResponse.json({ data: [] })
      }),
    )
    const auth = provider({
      providers: [{ provider: 'google', label: 'Google' }],
    })
    await auth.discoverProviders()
    await auth.discoverProviders()

    expect(calls).toBe(1)
    expect(auth.providers.value).toEqual([])
  })
})

describe('handing a sign-in to an external browser', () => {
  const shellProvider = (
    navigate: (url: string) => void | Promise<void>,
  ): ReturnType<typeof createOAuthAuthProvider> =>
    createOAuthAuthProvider({
      providers: [{ provider: 'google', label: 'Google' }],
      baseUrl: BASE_URL,
      clientId: 'axon-desktop',
      redirectUri: 'org.matrixaxon.axon:/oauth/callback',
      storage: memoryStorage(),
      pendingStorage: memoryStorage(),
      navigate,
    })

  it('reports that sign-in leaves the app only when it does', () => {
    // The browser's default navigation replaces this page and never returns,
    // so there is no caller left to tell anything to. An injected one hands
    // off to another application and comes straight back.
    expect(shellProvider(() => {}).signInLeavesTheApp).toBe(true)
    expect(
      createOAuthAuthProvider({
        providers: [],
        baseUrl: BASE_URL,
        storage: memoryStorage(),
        pendingStorage: memoryStorage(),
      }).signInLeavesTheApp,
    ).toBe(false)
  })

  it('fails the sign-in when the browser could not be opened', async () => {
    // `openExternal` used to swallow this, so a denied capability scope or an
    // absent handler resolved `startSignIn` as though a sign-in had begun.
    const auth = shellProvider(() =>
      Promise.reject(new Error('could not open an external link (https://x)')),
    )

    await expect(auth.startSignIn('google')).rejects.toThrow(
      'could not open an external link',
    )
  })

  it('does not leave the buttons latched once the browser has the URL', async () => {
    // The bug this replaces: the button set `busy` and cleared it only if
    // `startSignIn` rejected. Handing off to a browser resolves, so closing
    // that browser, declining at the provider, or getting a failed callback
    // left every button disabled reading "Opening..." until the app was
    // restarted. Nothing in this app is pending at that point.
    const auth = shellProvider(() => {})
    const { findByRole, getByRole } = render(<auth.LoginBootstrap />)

    const button = (await findByRole('button', {
      name: /google/i,
    })) as HTMLButtonElement
    fireEvent.click(button)

    await waitFor(() => expect(button.disabled).toBe(false))
    expect(getByRole('status').textContent).toMatch(/continue signing in/i)

    // And it can simply be used again.
    fireEvent.click(button)
    await waitFor(() => expect(button.disabled).toBe(false))
  })
})
