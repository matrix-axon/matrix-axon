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
import { NativeSignInCancelled } from '../platform'
import { memoryStorage } from '../test/memory-storage'
import { createOAuthAuthProvider, LinkAppleSection } from './oauth'

const BASE_URL = 'http://axon.test'
const PROVIDERS_URL = `${BASE_URL}/v1/oauth/providers`
const CHALLENGE_URL = `${BASE_URL}/v1/oauth/apple/native/challenge`
const NATIVE_TOKEN_URL = `${BASE_URL}/v1/oauth/apple/native/token`

// The server's nonce is a base64 SHA-256 digest; it has to reach Apple as-is.
const NONCE = 'q0Pz3oT8Xx2dYlC4mZ9M1b7iH5aVfE6wKuJrNgSx+/8='

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function providerRoutes(browser: string[], native: string[] | 'disabled') {
  return http.get(PROVIDERS_URL, ({ request }) => {
    const flow = new URL(request.url).searchParams.get('flow')
    if (flow === 'native') {
      return native === 'disabled'
        ? HttpResponse.json(
            { error: { code: 'not_found', message: 'disabled' } },
            { status: 404 },
          )
        : HttpResponse.json({
            data: native.map((provider) => ({
              provider,
              browser: browser.includes(provider),
              native: true,
            })),
          })
    }
    return HttpResponse.json({
      data: browser.map((provider) => ({
        provider,
        browser: true,
        native: false,
      })),
    })
  })
}

interface Recorded {
  challenge: { form: URLSearchParams; authorization: string | null }[]
  token: { form: URLSearchParams; authorization: string | null }[]
}

function nativeRoutes(recorded: Recorded) {
  return [
    http.post(CHALLENGE_URL, async ({ request }) => {
      recorded.challenge.push({
        form: new URLSearchParams(await request.text()),
        authorization: request.headers.get('authorization'),
      })
      return HttpResponse.json({
        challenge: 'challenge-secret',
        nonce: NONCE,
        expires_in: 300,
      })
    }),
    http.post(NATIVE_TOKEN_URL, async ({ request }) => {
      recorded.token.push({
        form: new URLSearchParams(await request.text()),
        authorization: request.headers.get('authorization'),
      })
      return HttpResponse.json({
        access_token: 'apple-access',
        token_type: 'Bearer',
        expires_in: 3600,
        refresh_token: 'apple-refresh',
      })
    }),
  ]
}

function newRecorded(): Recorded {
  return { challenge: [], token: [] }
}

describe('native Sign in with Apple', () => {
  it('offers Apple natively beside the browser providers', async () => {
    server.use(providerRoutes(['apple', 'google', 'microsoft'], ['apple']))
    const auth = createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      storage: memoryStorage(),
      appleSignIn: vi.fn(),
    })

    await auth.discoverProviders()

    expect(auth.providers.value).toEqual([
      { provider: 'apple', label: 'Apple', native: true },
      { provider: 'google', label: 'Google' },
      { provider: 'microsoft', label: 'Microsoft' },
    ])
    expect(auth.canBindApple.value).toBe(true)
  })

  it('adds Apple on a server that offers it to native apps only', async () => {
    server.use(providerRoutes(['google'], ['apple']))
    const auth = createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      storage: memoryStorage(),
      appleSignIn: vi.fn(),
    })

    await auth.discoverProviders()

    expect(auth.providers.value.map(({ provider }) => provider)).toEqual([
      'apple',
      'google',
    ])
  })

  it('keeps browser Apple when the server has no native Apple', async () => {
    server.use(providerRoutes(['apple', 'google'], []))
    const auth = createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      storage: memoryStorage(),
      appleSignIn: vi.fn(),
    })

    await auth.discoverProviders()

    expect(auth.providers.value).toEqual([
      { provider: 'apple', label: 'Apple' },
      { provider: 'google', label: 'Google' },
    ])
    expect(auth.canBindApple.value).toBe(false)
  })

  it('never asks for native providers without a native sheet', async () => {
    const flows: (string | null)[] = []
    server.use(
      http.get(PROVIDERS_URL, ({ request }) => {
        flows.push(new URL(request.url).searchParams.get('flow'))
        return HttpResponse.json({
          data: [{ provider: 'apple', browser: true, native: false }],
        })
      }),
    )
    const auth = createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      storage: memoryStorage(),
    })

    await auth.discoverProviders()

    expect(flows).toEqual([null])
    expect(auth.providers.value).toEqual([
      { provider: 'apple', label: 'Apple' },
    ])
  })

  it('signs in with the server nonce verbatim and redeems in place', async () => {
    const recorded = newRecorded()
    server.use(providerRoutes(['google'], ['apple']), ...nativeRoutes(recorded))
    const appleSignIn = vi.fn(() => Promise.resolve('apple-identity-token'))
    const navigate = vi.fn()
    const storage = memoryStorage()
    const auth = createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      clientId: 'axon-desktop',
      storage,
      navigate,
      appleSignIn,
    })
    await auth.discoverProviders()

    await expect(auth.startSignIn('apple')).resolves.toBe('signed-in')

    expect(appleSignIn).toHaveBeenCalledWith(NONCE)
    expect(navigate).not.toHaveBeenCalled()
    expect(recorded.challenge).toHaveLength(1)
    expect(Object.fromEntries(recorded.challenge[0].form)).toEqual({
      client_id: 'axon-desktop',
      purpose: 'login',
    })
    expect(recorded.challenge[0].authorization).toBeNull()
    expect(Object.fromEntries(recorded.token[0].form)).toEqual({
      client_id: 'axon-desktop',
      challenge: 'challenge-secret',
      identity_token: 'apple-identity-token',
    })
    expect(auth.signedIn.value).toBe(true)
    expect(await auth.getToken()).toBe('apple-access')
    const stored = JSON.parse(storage.getItem('axon.oauth.session') ?? '{}')
    expect(stored).toMatchObject({
      accessToken: 'apple-access',
      refreshToken: 'apple-refresh',
      provider: 'apple',
    })
  })

  it('redeems nothing when the Apple sheet is dismissed', async () => {
    const recorded = newRecorded()
    server.use(providerRoutes([], ['apple']), ...nativeRoutes(recorded))
    const auth = createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      storage: memoryStorage(),
      appleSignIn: () => Promise.reject(new NativeSignInCancelled('cancelled')),
    })
    await auth.discoverProviders()

    await expect(auth.startSignIn('apple')).rejects.toBeInstanceOf(
      NativeSignInCancelled,
    )

    expect(recorded.token).toHaveLength(0)
    expect(auth.signedIn.value).toBe(false)
  })

  it("reports the server's refusal of an unbound Apple ID", async () => {
    server.use(
      providerRoutes([], ['apple']),
      http.post(CHALLENGE_URL, () =>
        HttpResponse.json({ challenge: 'c', nonce: NONCE, expires_in: 300 }),
      ),
      http.post(NATIVE_TOKEN_URL, () =>
        HttpResponse.json(
          {
            error: 'invalid_grant',
            error_description:
              'this Apple ID is not linked to an Axon owner; link it first',
          },
          { status: 400 },
        ),
      ),
    )
    const auth = createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      storage: memoryStorage(),
      appleSignIn: () => Promise.resolve('token'),
    })
    await auth.discoverProviders()

    await expect(auth.startSignIn('apple')).rejects.toThrow(
      'this Apple ID is not linked to an Axon owner; link it first',
    )
    expect(auth.signedIn.value).toBe(false)
  })

  it('binds with the owner bearer on both requests and adopts the session', async () => {
    const recorded = newRecorded()
    server.use(providerRoutes([], ['apple']), ...nativeRoutes(recorded))
    const auth = createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      clientId: 'axon-desktop',
      storage: memoryStorage(),
      appleSignIn: () => Promise.resolve('apple-identity-token'),
    })

    await auth.bindApple('owner-token')

    expect(recorded.challenge[0].form.get('purpose')).toBe('bind')
    expect(recorded.challenge[0].authorization).toBe('Bearer owner-token')
    expect(recorded.token[0].authorization).toBe('Bearer owner-token')
    expect(await auth.getToken()).toBe('apple-access')
  })
})

describe('the Apple button', () => {
  it('shows nothing after the sheet is dismissed, and can be used again', async () => {
    server.use(
      providerRoutes([], ['apple']),
      http.post(CHALLENGE_URL, () =>
        HttpResponse.json({ challenge: 'c', nonce: NONCE, expires_in: 300 }),
      ),
    )
    const auth = createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      storage: memoryStorage(),
      navigate: vi.fn(),
      appleSignIn: () => Promise.reject(new NativeSignInCancelled('cancelled')),
    })
    const view = render(<auth.LoginBootstrap />)
    const button = await view.findByRole('button', {
      name: /Sign in with Apple/,
    })

    fireEvent.click(button)

    await waitFor(() => expect(button.hasAttribute('disabled')).toBe(false))
    expect(view.container.querySelector('.error')).toBeNull()
    expect(view.queryByText(/Continue signing in in your browser/)).toBeNull()
  })
})

describe('LinkAppleSection', () => {
  it('renders nothing where native Apple is not offered', async () => {
    server.use(providerRoutes(['google'], 'disabled'))
    const auth = createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      storage: memoryStorage(),
      appleSignIn: vi.fn(),
    })
    const view = render(
      <LinkAppleSection oauth={auth} bearer={() => 'owner'} />,
    )
    await auth.discoverProviders()

    expect(view.container.textContent).toBe('')
  })

  it('links the Apple ID using the current bearer', async () => {
    const recorded = newRecorded()
    server.use(providerRoutes([], ['apple']), ...nativeRoutes(recorded))
    const auth = createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      storage: memoryStorage(),
      appleSignIn: () => Promise.resolve('apple-identity-token'),
    })
    const view = render(
      <LinkAppleSection oauth={auth} bearer={() => 'owner-token'} />,
    )

    fireEvent.click(
      await view.findByRole('button', { name: 'Link an Apple ID' }),
    )

    // Linking adopts the Apple session, which is itself the proof of the link.
    await view.findByText(/You are signed in with Apple/)
    expect(view.queryByRole('button')).toBeNull()
    expect(recorded.challenge[0].authorization).toBe('Bearer owner-token')
  })

  it('offers no link button when already signed in with Apple', async () => {
    server.use(providerRoutes([], ['apple']))
    const storage = memoryStorage({
      'axon.oauth.session': JSON.stringify({
        accessToken: 'a',
        refreshToken: 'r',
        expiresAt: Date.now() + 3_600_000,
        provider: 'apple',
      }),
    })
    const auth = createOAuthAuthProvider({
      providers: [],
      baseUrl: BASE_URL,
      storage,
      appleSignIn: vi.fn(),
    })
    const view = render(<LinkAppleSection oauth={auth} bearer={() => 'a'} />)

    await view.findByText(/your Apple ID is linked/)
    expect(view.queryByRole('button')).toBeNull()
  })
})
