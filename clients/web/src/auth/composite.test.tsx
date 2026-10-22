import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { afterEach } from 'vitest'
import { describe, expect, it } from 'vitest'
import { memoryStorage } from '../test/memory-storage'
import { createCompositeAuthProvider } from './composite'

afterEach(cleanup)

describe('createCompositeAuthProvider', () => {
  it('uses a pasted token when there is no OAuth session', () => {
    const storage = memoryStorage({ 'axon.token': 'manual-token' })
    const auth = createCompositeAuthProvider({
      providers: [],
      baseUrl: 'http://axon.test',
      storage,
      pendingStorage: memoryStorage(),
    })

    expect(auth.signedIn.value).toBe(true)
    expect(auth.getToken()).toBe('manual-token')
  })

  it('prefers the OAuth access token over a pasted token', () => {
    const storage = memoryStorage({
      'axon.token': 'manual-token',
      'axon.oauth.session': JSON.stringify({
        accessToken: 'oauth-token',
        refreshToken: 'refresh-token',
        expiresAt: Date.now() + 3600_000,
        provider: 'google',
      }),
    })
    const auth = createCompositeAuthProvider({
      providers: [{ provider: 'google', label: 'Google' }],
      baseUrl: 'http://axon.test',
      storage,
      pendingStorage: memoryStorage(),
    })

    expect(auth.getToken()).toBe('oauth-token')
  })

  it('clears both OAuth and pasted-token state on sign-out', () => {
    const storage = memoryStorage({
      'axon.token': 'manual-token',
      'axon.oauth.session': JSON.stringify({
        accessToken: 'oauth-token',
        refreshToken: 'refresh-token',
        expiresAt: Date.now() + 3600_000,
        provider: 'google',
      }),
    })
    const pending = memoryStorage({ 'axon.oauth.pending': '{}' })
    const auth = createCompositeAuthProvider({
      providers: [{ provider: 'google', label: 'Google' }],
      baseUrl: 'http://axon.test',
      storage,
      pendingStorage: pending,
    })

    auth.clearToken()

    expect(auth.signedIn.value).toBe(false)
    expect(storage.getItem('axon.token')).toBeNull()
    expect(storage.getItem('axon.oauth.session')).toBeNull()
    expect(pending.getItem('axon.oauth.pending')).toBeNull()
  })

  it('waits for a sign-out hook before dropping the session', async () => {
    const storage = memoryStorage({ 'axon.token': 'manual-token' })
    const auth = createCompositeAuthProvider({
      providers: [],
      baseUrl: 'http://axon.test',
      storage,
      pendingStorage: memoryStorage(),
    })
    let release: () => void = () => {}
    auth.onBeforeSignOut(
      () =>
        new Promise<void>((resolve) => {
          release = resolve
        }),
    )

    const pending = auth.clearToken()
    expect(auth.signedIn.value).toBe(true)
    release()
    await pending
    expect(auth.signedIn.value).toBe(false)
    expect(storage.getItem('axon.token')).toBeNull()
  })

  it('keeps the session when a sign-out hook fails', async () => {
    const storage = memoryStorage({ 'axon.token': 'manual-token' })
    const auth = createCompositeAuthProvider({
      providers: [],
      baseUrl: 'http://axon.test',
      storage,
      pendingStorage: memoryStorage(),
    })
    auth.onBeforeSignOut(() => Promise.reject(new Error('still registered')))

    await expect(auth.clearToken()).resolves.toBe(false)
    expect(auth.signedIn.value).toBe(true)
    expect(storage.getItem('axon.token')).toBe('manual-token')
  })

  it('lets the user choose a one-time pasted-token login', () => {
    const storage = memoryStorage()
    const sessionStorage = memoryStorage()
    const auth = createCompositeAuthProvider({
      providers: [],
      baseUrl: 'http://axon.test',
      storage,
      sessionStorage,
      pendingStorage: memoryStorage(),
    })

    const { getByLabelText, getByRole } = render(<auth.LoginBootstrap />)
    fireEvent.click(getByLabelText('Remember me'))
    fireEvent.input(getByLabelText('Access token'), {
      target: { value: 'tok-session' },
    })
    fireEvent.click(getByRole('button', { name: 'Sign in' }))

    expect(auth.getToken()).toBe('tok-session')
    expect(storage.getItem('axon.token')).toBeNull()
    expect(sessionStorage.getItem('axon.token')).toBe('tok-session')
  })

  it('renders configured SSO buttons before the token-paste fallback', async () => {
    let redirect: string | null = null
    const auth = createCompositeAuthProvider({
      providers: [
        { provider: 'google', label: 'Google' },
        { provider: 'microsoft', label: 'Microsoft' },
        { provider: 'apple', label: 'Apple' },
        { provider: 'test-idp', label: 'Test IdP' },
      ],
      baseUrl: 'http://axon.test',
      storage: memoryStorage(),
      pendingStorage: memoryStorage(),
      navigate: (url) => {
        redirect = url
      },
    })

    const { getByRole } = render(<auth.LoginBootstrap />)
    const google = getByRole('button', { name: 'Continue with Google' })
    expect(google.className).toContain('sso-google')
    expect(
      getByRole('button', { name: 'Sign in with Microsoft' }).className,
    ).toContain('sso-microsoft')
    expect(
      getByRole('button', { name: 'Sign in with Apple' }).className,
    ).toContain('sso-apple')
    expect(
      getByRole('button', { name: 'Continue with Test IdP' }).className,
    ).toContain('sso-generic')

    fireEvent.click(google)

    await waitFor(() => expect(redirect).not.toBeNull())
    const url = new URL(redirect!)
    expect(url.pathname).toBe('/v1/oauth/authorize')
    expect(url.searchParams.get('provider')).toBe('google')
    expect(url.searchParams.get('client_id')).toBe('axon-web')
    expect(getByRole('button', { name: 'Sign in' })).toBeTruthy()
  })

  it('carries the one-time choice into an OAuth redirect', async () => {
    let redirect: string | null = null
    const pendingStorage = memoryStorage()
    const auth = createCompositeAuthProvider({
      providers: [{ provider: 'google', label: 'Google' }],
      baseUrl: 'http://axon.test',
      storage: memoryStorage(),
      sessionStorage: memoryStorage(),
      pendingStorage,
      navigate: (url) => {
        redirect = url
      },
    })

    const { getByLabelText, getByRole } = render(<auth.LoginBootstrap />)
    fireEvent.click(getByLabelText('Remember me'))
    fireEvent.click(getByRole('button', { name: 'Continue with Google' }))

    await waitFor(() => expect(redirect).not.toBeNull())
    const pending = JSON.parse(pendingStorage.getItem('axon.oauth.pending')!)
    expect(pending.storageMode).toBe('session')
  })
})
