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
    expect(f.call).toHaveBeenCalledWith('invalidate')
    expect(f.auth.signedIn.value).toBe(false)
    f.stop()
  })
})
