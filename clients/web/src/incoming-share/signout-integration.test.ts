import { afterEach, expect, it, vi } from 'vitest'
import { createServices } from '../services'
import { browserPlatform } from '../platform'
import { memoryStorage } from '../test/memory-storage'
import * as push from '../push/registration'

afterEach(() => vi.restoreAllMocks())

it('deferred push sign-out completion rechecks the native sharing barrier', async () => {
  let captured: push.PushRegistrationDeps | undefined
  vi.spyOn(push, 'createPushRegistration').mockImplementation((deps) => {
    captured = deps
    return { reconcile: async () => {}, dispose() {} }
  })
  let unavailable = true
  const call = vi.fn(async (action: string) => {
    if (action === 'list') return []
    if (action === 'invalidate' && unavailable) throw { code: 'invalidation' }
    return {}
  })
  const storage = Object.assign(
    memoryStorage({ 'axon.token': 'synthetic-review-token' }),
    { settled: () => Promise.resolve() },
  )
  const services = createServices(memoryStorage(), memoryStorage(), {
    ...browserPlatform(),
    secureStorage: storage,
    incomingShare: { call: call as never, subscribe: () => () => {} },
    remotePush: {
      provider: 'apns',
      deviceToken: async () => ({
        token: 'synthetic-device',
        environment: 'sandbox',
      }),
      deliveredEventIds: async () => [],
      onWake: () => () => {},
      postCollapsed: async () => {},
    },
  })
  expect(services.auth.signedIn.value).toBe(true)
  captured!.finishSignOut()
  await vi.waitFor(() =>
    expect(call.mock.calls.some(([action]) => action === 'invalidate')).toBe(
      true,
    ),
  )
  expect(services.auth.signedIn.value).toBe(true)
  unavailable = false
  captured!.finishSignOut()
  await vi.waitFor(() => expect(services.auth.signedIn.value).toBe(false))
  services.incomingShares?.dispose()
  services.live.stop()
})
