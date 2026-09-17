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
import type { AuthProvider } from '../auth/provider'
import {
  apiErrorCode,
  apiErrorMessage,
  createApiClient,
  isErrorEnvelope,
  REQUEST_TIMEOUT_MESSAGE,
  REQUEST_UNREACHABLE_MESSAGE,
  requestFailureMessage,
} from './client'

const BASE_URL = 'http://axon.test'

const ACCOUNT = {
  account_id: '6b53f7f0-0000-4000-8000-000000000001',
  user_id: '@alice:example.org',
  homeserver_url: 'https://matrix.example.org',
  state: 'active',
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
}

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function stubAuth(
  token: string | null | Promise<string | null>,
): AuthProvider & {
  failures: number
} {
  const auth = {
    failures: 0,
    getToken: () => token,
    onAuthFailure() {
      auth.failures += 1
    },
    LoginBootstrap: () => null,
  }
  return auth
}

describe('createApiClient', () => {
  it('attaches the bearer token and round-trips GET /v1/accounts', async () => {
    let seenAuthorization: string | null = null
    server.use(
      http.get(`${BASE_URL}/v1/accounts`, ({ request }) => {
        seenAuthorization = request.headers.get('authorization')
        return HttpResponse.json({ data: [ACCOUNT] })
      }),
    )

    const api = createApiClient(stubAuth('tok-123'), BASE_URL)
    const { data, error, response } = await api.GET('/v1/accounts')

    expect(seenAuthorization).toBe('Bearer tok-123')
    expect(response.status).toBe(200)
    expect(error).toBeUndefined()
    expect(data?.data).toEqual([ACCOUNT])
  })

  it('awaits an async token provider (the OAuth-shaped seam)', async () => {
    let seenAuthorization: string | null = null
    server.use(
      http.get(`${BASE_URL}/v1/accounts`, ({ request }) => {
        seenAuthorization = request.headers.get('authorization')
        return HttpResponse.json({ data: [] })
      }),
    )

    const api = createApiClient(
      stubAuth(Promise.resolve('tok-async')),
      BASE_URL,
    )
    await api.GET('/v1/accounts')

    expect(seenAuthorization).toBe('Bearer tok-async')
  })

  it('sends no Authorization header when signed out', async () => {
    let seenAuthorization: string | null = 'unset'
    server.use(
      http.get(`${BASE_URL}/v1/accounts`, ({ request }) => {
        seenAuthorization = request.headers.get('authorization')
        return HttpResponse.json({ data: [] })
      }),
    )

    await createApiClient(stubAuth(null), BASE_URL).GET('/v1/accounts')

    expect(seenAuthorization).toBeNull()
  })

  it('reports a 401 to the provider and surfaces the error envelope', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/accounts`, () =>
        HttpResponse.json(
          {
            error: {
              code: 'unauthorized',
              message: 'invalid or revoked token',
            },
          },
          {
            status: 401,
            headers: { 'www-authenticate': 'Bearer error="invalid_token"' },
          },
        ),
      ),
    )

    const auth = stubAuth('revoked-token')
    const api = createApiClient(auth, BASE_URL)
    const { data, error, response } = await api.GET('/v1/accounts')

    expect(auth.failures).toBe(1)
    expect(response.status).toBe(401)
    expect(data).toBeUndefined()
    expect(apiErrorCode(error)).toBe('unauthorized')
    expect(apiErrorMessage(error)).toBe('invalid or revoked token')
  })

  it('does not report non-401 errors to the provider', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/accounts`, () =>
        HttpResponse.json(
          { error: { code: 'internal', message: 'boom' } },
          { status: 500 },
        ),
      ),
    )

    const auth = stubAuth('tok-123')
    const { error } = await createApiClient(auth, BASE_URL).GET('/v1/accounts')

    expect(auth.failures).toBe(0)
    expect(apiErrorCode(error)).toBe('internal')
  })

  it('parameterizes path templates', async () => {
    const accountId = ACCOUNT.account_id
    server.use(
      http.get(`${BASE_URL}/v1/accounts/${accountId}`, () =>
        HttpResponse.json({ data: ACCOUNT }),
      ),
    )

    const api = createApiClient(stubAuth('tok-123'), BASE_URL)
    const { data } = await api.GET('/v1/accounts/{account_id}', {
      params: { path: { account_id: accountId } },
    })

    expect(data?.data.user_id).toBe('@alice:example.org')
  })
})

describe('error envelope helpers', () => {
  it('recognizes the server envelope and rejects other shapes', () => {
    const envelope = {
      error: { code: 'not_found', message: 'route not found' },
    }
    expect(isErrorEnvelope(envelope)).toBe(true)
    for (const other of [
      null,
      undefined,
      'oops',
      {},
      { error: 'oops' },
      { error: {} },
    ]) {
      expect(isErrorEnvelope(other)).toBe(false)
    }
  })

  it('falls back for non-envelope errors', () => {
    expect(apiErrorCode(undefined)).toBeNull()
    expect(apiErrorMessage('<html>proxy error</html>')).toBe(
      'unexpected server response',
    )
  })
})

describe('the transport seam (ADR 0102 § 2)', () => {
  /**
   * `createApiClient` defaults to `browserPlatform()`, so a change that stopped
   * threading the injected platform through would leave every other test in
   * this file green — msw intercepts the global `fetch` either way. These
   * assert the injected function is the one that runs.
   */
  it('issues requests through the injected fetch, not the global', async () => {
    const calls: string[] = []
    const injected: typeof globalThis.fetch = async (input) => {
      calls.push(String(input instanceof Request ? input.url : input))
      return new Response(JSON.stringify({ data: [ACCOUNT] }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      })
    }

    const api = createApiClient(stubAuth('tok-1'), BASE_URL, {
      fetch: injected,
    })
    const { data } = await api.GET('/v1/accounts')

    // No msw handler is registered, and the server is set to error on any
    // unhandled request — so a global-fetch fallback would fail this outright.
    expect(calls).toHaveLength(1)
    expect(calls[0]).toContain('/v1/accounts')
    expect(data?.data?.[0]?.account_id).toBe(ACCOUNT.account_id)
  })

  it('still carries the bearer token when the platform is injected', async () => {
    let seen: string | null = null
    const injected: typeof globalThis.fetch = async (input, init) => {
      const headers = new Headers(
        input instanceof Request ? input.headers : init?.headers,
      )
      seen = headers.get('authorization')
      return new Response(JSON.stringify({ data: [] }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      })
    }

    const api = createApiClient(stubAuth('tok-2'), BASE_URL, {
      fetch: injected,
    })
    await api.GET('/v1/accounts')

    expect(seen).toBe('Bearer tok-2')
  })

  /**
   * The iPhone report behind this: a request issued across a WiFi→cell
   * handover never settles, because the connection it went out on no longer
   * has a route and nothing ever tears it down. Every caller copes with a
   * *rejected* request; none copes with one that never answers, so the room
   * list and the timeline sit on their placeholders until the app is
   * relaunched.
   */
  it('rejects a request that never answers, instead of hanging forever', async () => {
    server.use(http.get(`${BASE_URL}/v1/accounts`, () => new Promise(() => {})))

    const api = createApiClient(stubAuth('tok-123'), BASE_URL, undefined, 40)

    await expect(api.GET('/v1/accounts')).rejects.toThrow()
  })

  it('leaves a request that answers in time alone', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/accounts`, () =>
        HttpResponse.json({ data: [ACCOUNT] }),
      ),
    )

    const api = createApiClient(stubAuth('tok-123'), BASE_URL, undefined, 5_000)
    const { data, error } = await api.GET('/v1/accounts')

    expect(error).toBeUndefined()
    expect(data?.data).toEqual([ACCOUNT])
  })

  /**
   * The QR stores pass their own 15 s signal per call and read
   * `controller.signal.aborted` to tell their timeout apart from a transport
   * failure. Replacing their signal rather than combining with it would make
   * every one of those calls unabortable.
   */
  it("keeps the caller's own abort working", async () => {
    server.use(http.get(`${BASE_URL}/v1/accounts`, () => new Promise(() => {})))

    const api = createApiClient(stubAuth('tok-123'), BASE_URL, undefined, 5_000)
    const controller = new AbortController()
    const pending = api.GET('/v1/accounts', { signal: controller.signal })
    controller.abort()

    await expect(pending).rejects.toThrow()
    expect(controller.signal.aborted).toBe(true)
  })

  it('still sends the bearer token on a request it re-signed', async () => {
    let seenAuthorization: string | null = null
    server.use(
      http.get(`${BASE_URL}/v1/accounts`, ({ request }) => {
        seenAuthorization = request.headers.get('authorization')
        return HttpResponse.json({ data: [ACCOUNT] })
      }),
    )

    const api = createApiClient(
      stubAuth('tok-deadline'),
      BASE_URL,
      undefined,
      5_000,
    )
    await api.GET('/v1/accounts')

    expect(seenAuthorization).toBe('Bearer tok-deadline')
  })

  /**
   * The deadline is attached by rebuilding the request, because
   * `Request.signal` is read-only. A rebuild that dropped the body would break
   * every mutation in the client while leaving reads working — so the body is
   * what this asserts, not the signal.
   */
  it('carries a request body through the re-signed request', async () => {
    let seenBody: unknown = null
    server.use(
      http.post(`${BASE_URL}/v1/accounts/login`, async ({ request }) => {
        seenBody = await request.json()
        return HttpResponse.json({ data: ACCOUNT }, { status: 201 })
      }),
    )

    const api = createApiClient(stubAuth('tok-123'), BASE_URL, undefined, 5_000)
    await api.POST('/v1/accounts/login', {
      body: {
        username: '@alice:example.org',
        password: 'hunter2',
        homeserver_url: 'https://matrix.example.org',
      },
    })

    expect(seenBody).toMatchObject({ username: '@alice:example.org' })
  })

  /**
   * The hole the first version of this deadline had. `auth.getToken()` can go
   * to the network itself — the OAuth provider refreshes a near-expiry access
   * token by POSTing the token endpoint — so a deadline attached to the
   * outgoing request *after* awaiting the token never gets a chance to fire:
   * the middleware never returns, `fetch` is never called, and the request
   * hangs indefinitely while looking, in the bundle, exactly like a request
   * that has a deadline.
   */
  it('gives up when the token itself never arrives', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/accounts`, () =>
        HttpResponse.json({ data: [ACCOUNT] }),
      ),
    )
    const hangingAuth: AuthProvider = {
      getToken: () => new Promise<string | null>(() => {}),
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    }

    const api = createApiClient(hangingAuth, BASE_URL, undefined, 40)

    await expect(api.GET('/v1/accounts')).rejects.toThrow()
  })

  it('does not leave a rejection behind when the token wins the race', async () => {
    server.use(
      http.get(`${BASE_URL}/v1/accounts`, () =>
        HttpResponse.json({ data: [ACCOUNT] }),
      ),
    )
    const unhandled = vi.fn()
    process.on('unhandledRejection', unhandled)
    try {
      // Deadline shorter than the wait that follows, so it fires well after
      // the token resolved and the race was already decided.
      const api = createApiClient(stubAuth('tok-123'), BASE_URL, undefined, 20)
      await api.GET('/v1/accounts')
      await new Promise((resolve) => setTimeout(resolve, 80))
    } finally {
      process.off('unhandledRejection', unhandled)
    }

    expect(unhandled).not.toHaveBeenCalled()
  })
})

describe('requestFailureMessage', () => {
  /**
   * What the reader used to be shown for our own 20 s deadline on an iPhone.
   * WebKit rejects with a generic `AbortError` rather than the spec's
   * `TimeoutError`, so both names have to land here or the engine the reports
   * come from is the one engine this does not cover.
   */
  it('rewords both abort names as a timeout', () => {
    for (const name of ['AbortError', 'TimeoutError']) {
      expect(
        requestFailureMessage(new DOMException('Fetch is aborted', name)),
      ).toBe(REQUEST_TIMEOUT_MESSAGE)
    }
  })

  /**
   * A `DOMException` is *not* `instanceof Error` under jsdom, though it is in
   * a browser. Classifying by that would pass on a hand-built `Error` and take
   * the wrong branch on the real thing — so the name is read structurally, and
   * this asserts the real type rather than a stand-in.
   */
  it('classifies a DOMException, which is not an Error here', () => {
    const abort = new DOMException('Fetch is aborted', 'AbortError')

    expect(abort instanceof Error).toBe(false)
    expect(requestFailureMessage(abort)).toBe(REQUEST_TIMEOUT_MESSAGE)
  })

  it("rewords fetch's transport failure as unreachable", () => {
    expect(requestFailureMessage(new TypeError('Load failed'))).toBe(
      REQUEST_UNREACHABLE_MESSAGE,
    )
  })

  it('leaves a real error its own message', () => {
    expect(requestFailureMessage(new Error('room is not encrypted'))).toBe(
      'room is not encrypted',
    )
  })

  it('has something to say about a thrown non-error', () => {
    expect(requestFailureMessage('nope')).toBe(REQUEST_UNREACHABLE_MESSAGE)
    expect(requestFailureMessage(undefined)).toBe(REQUEST_UNREACHABLE_MESSAGE)
  })

  it('reaches a caller through the client, not just in isolation', async () => {
    server.use(http.get(`${BASE_URL}/v1/accounts`, () => new Promise(() => {})))
    const api = createApiClient(stubAuth('tok-123'), BASE_URL, undefined, 40)

    await expect(api.GET('/v1/accounts')).rejects.toThrow(
      REQUEST_TIMEOUT_MESSAGE,
    )
  })
})
