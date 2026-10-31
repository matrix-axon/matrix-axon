import { describe, expect, it, vi } from 'vitest'
import { createCompositeAuthProvider } from '../auth/composite'
import { memoryStorage } from '../test/memory-storage'
import type { SecureStorage } from '../platform'
import type { IncomingSharePort } from './types'
import type { IncomingShareStore } from './store'
import { connectIncomingShareSession } from './session'

function fixture(sessionOnly = false, oauth = false) {
  const oauthSession = JSON.stringify({
    accessToken: 'old-access',
    refreshToken: 'private-refresh',
    expiresAt: Date.now() - 1,
    provider: 'google',
  })
  const storage = Object.assign(
    memoryStorage(
      sessionOnly
        ? {}
        : oauth
          ? { 'axon.oauth.session': oauthSession }
          : { 'axon.token': 'persisted' },
    ),
    { settled: () => Promise.resolve() },
  ) satisfies SecureStorage
  const auth = createCompositeAuthProvider({
    providers: [],
    baseUrl: 'https://axon.test',
    storage,
    platform: {
      fetch: vi.fn().mockImplementation(
        async () =>
          new Response(
            JSON.stringify({
              access_token: 'renewed-access',
              refresh_token: 'renewed-private-refresh',
              token_type: 'Bearer',
              expires_in: 3600,
            }),
            { status: 200, headers: { 'Content-Type': 'application/json' } },
          ),
      ),
    },
    sessionStorage: memoryStorage(
      sessionOnly
        ? oauth
          ? { 'axon.oauth.session': oauthSession }
          : { 'axon.token': 'one-time' }
        : {},
    ),
  })
  const call = vi.fn().mockResolvedValue({})
  const port = { call, subscribe: () => () => {} } as IncomingSharePort
  let ready: () => Promise<void> = () => Promise.resolve()
  const store = {
    setSessionReady(wait: () => Promise<void>) {
      ready = wait
    },
    refresh: vi.fn(),
    reset: vi.fn(),
    error: { value: null },
  } as unknown as IncomingShareStore
  const stop = connectIncomingShareSession(
    port,
    store,
    auth,
    storage,
    'https://axon.test',
  )
  return { call, auth, stop, store, ready: () => ready() }
}
describe('shared session publication', () => {
  it('never publishes a hidden pasted credential while clearing an OAuth session', async () => {
    const f = fixture(false, true)
    f.auth.tokenPaste.setToken('hidden-pasted-access')
    await f.ready()
    f.call.mockClear()
    await f.auth.clearToken()
    await f.ready()
    expect(
      f.call.mock.calls
        .filter(([action]) => action === 'publish')
        .every(([, args]) => args.token === null),
    ).toBe(true)
    f.stop()
  })
  it.each(['silent', 'manual'] as const)(
    'republishes %s OAuth renewal without replacing identity',
    async (kind) => {
      const f = fixture(false, true)
      await f.ready()
      const identity = f.auth.sessionId.value
      f.call.mockClear()
      if (kind === 'silent') await f.auth.getToken()
      else await f.auth.refreshNow()
      await f.ready()
      expect(f.auth.sessionId.value).toBe(identity)
      expect(f.call).toHaveBeenCalledWith(
        'publish',
        expect.objectContaining({
          token: 'renewed-access',
          expiresAt: expect.any(Number),
          replace: false,
        }),
      )
      expect(JSON.stringify(f.call.mock.calls)).not.toContain('private-refresh')
      f.stop()
    },
  )
  it('does not persist a renewed session-only OAuth token', async () => {
    const f = fixture(true, true)
    await f.ready()
    await f.auth.getToken()
    await f.ready()
    expect(
      f.call.mock.calls
        .filter(([action]) => action === 'publish')
        .every(([, args]) => args.token === null),
    ).toBe(true)
    f.stop()
  })
  it('restores shared authority when another sign-out hook rejects after invalidation', async () => {
    const f = fixture()
    await f.ready()
    let fail!: () => void
    f.auth.onBeforeSignOut(
      () =>
        new Promise<void>((_, reject) => {
          fail = () => reject(new Error('push failed'))
        }),
    )
    const pending = f.auth.clearToken()
    await vi.waitFor(() => expect(f.call).toHaveBeenCalledWith('invalidate'))
    f.call.mockClear()
    fail()
    expect(await pending).toBe(false)
    await f.ready()
    expect(f.call).toHaveBeenCalledWith(
      'publish',
      expect.objectContaining({ token: 'persisted' }),
    )
    expect(f.auth.signedIn.value).toBe(true)
    f.stop()
  })
  it('waits for late revocation before restoring an aborted sign-out', async () => {
    const f = fixture()
    await f.ready()
    let revoke!: () => void
    f.call.mockImplementation(async (action) => {
      if (action === 'invalidate')
        await new Promise<void>((resolve) => {
          revoke = resolve
        })
      return {}
    })
    f.auth.onBeforeSignOut(() => Promise.reject(new Error('push failed early')))
    let finished = false
    const pending = Promise.resolve(f.auth.clearToken()).then((result) => {
      finished = true
      return result
    })
    await vi.waitFor(() => expect(revoke).toBeTypeOf('function'))
    await Promise.resolve()
    expect(finished).toBe(false)
    f.call.mockClear()
    revoke()
    expect(await pending).toBe(false)
    expect(f.call).toHaveBeenCalledWith(
      'publish',
      expect.objectContaining({ token: 'persisted' }),
    )
    f.stop()
  })
  it('sign-out survives publication and cleanup failure after native revocation', async () => {
    const f = fixture()
    await f.ready()
    f.call.mockImplementation(async (action) => {
      if (action === 'publish') throw { code: 'unavailable' }
      return {}
    })
    expect(await f.auth.clearToken()).toBe(true)
    expect(f.auth.signedIn.value).toBe(false)
    await f.ready().catch(() => {})
    f.stop()
  })
  it('reports a failure to commit either revocation barrier and permits retry', async () => {
    const f = fixture()
    await f.ready()
    f.call.mockRejectedValue({ code: 'invalidation' })
    expect(await f.auth.clearToken()).toBe(false)
    expect(f.auth.signedIn.value).toBe(true)
    expect(f.store.error.value).toContain('Could not disable shared access')
    f.call.mockResolvedValue({})
    expect(await f.auth.clearToken()).toBe(true)
    f.stop()
  })

  it('publishes persisted access credentials with no refresh credential', async () => {
    const f = fixture()
    await f.ready()
    expect(f.call).toHaveBeenCalledWith(
      'publish',
      expect.objectContaining({ token: 'persisted', replace: false }),
    )
    expect(JSON.stringify(f.call.mock.calls)).not.toContain('refreshToken')
    f.stop()
  })
  it('never promotes a session-only credential', async () => {
    const f = fixture(true)
    await f.ready()
    expect(f.call).toHaveBeenCalledWith(
      'publish',
      expect.objectContaining({ token: null, stagingAllowed: true }),
    )
    f.stop()
  })
  it('fences native work before completing sign-out', async () => {
    const f = fixture()
    await f.ready()
    await f.auth.clearToken()
    await f.ready()
    expect(f.store.reset).toHaveBeenCalled()
    expect(f.call).toHaveBeenCalledWith('invalidate')
    expect(f.auth.signedIn.value).toBe(false)
    f.stop()
  })
})
