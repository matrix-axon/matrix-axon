import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import type { AuthProvider } from '../auth/provider'
import {
  apiErrorCode,
  apiErrorMessage,
  createApiClient,
  isErrorEnvelope,
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
})
