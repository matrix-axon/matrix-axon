import { describe, expect, it, vi } from 'vitest'
import { createCompositeAuthProvider } from '../auth/composite'
import { memoryStorage } from '../test/memory-storage'
import type { SecureStorage } from '../platform'
import type { IncomingSharePort } from './types'
import type { IncomingShareStore } from './store'
import { connectIncomingShareSession } from './session'

function fixture(sessionOnly = false) {
  const storage = Object.assign(
    memoryStorage(sessionOnly ? {} : { 'axon.token': 'persisted' }),
    { settled: () => Promise.resolve() },
  ) satisfies SecureStorage
  const auth = createCompositeAuthProvider({
    providers: [],
    baseUrl: 'https://axon.test',
    storage,
    sessionStorage: memoryStorage(
      sessionOnly ? { 'axon.token': 'one-time' } : {},
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
      expect.objectContaining({ token: null }),
    )
    f.stop()
  })
  it('fences native work before completing sign-out', async () => {
    const f = fixture()
    await f.ready()
    await f.auth.clearToken()
    await f.ready()
    expect(f.store.reset).toHaveBeenCalled()
    expect(f.call).toHaveBeenCalledWith('publish', {
      server: 'https://axon.test',
      token: null,
      replace: true,
    })
    expect(f.auth.signedIn.value).toBe(false)
    f.stop()
  })
})
