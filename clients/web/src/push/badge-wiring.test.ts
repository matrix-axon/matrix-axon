import { afterEach, expect, it, vi } from 'vitest'
import { createServices } from '../services'
import { browserPlatform } from '../platform'
import { memoryStorage } from '../test/memory-storage'
import * as push from './registration'

afterEach(() => vi.restoreAllMocks())

// ADR 0129: the app-icon badge setting is also the push registration's
// `badge`, and changing it re-registers the device.
it('re-registers with the current badge setting when it changes', async () => {
  let captured: push.PushRegistrationDeps | undefined
  const reconcile = vi.fn(async () => {})
  vi.spyOn(push, 'createPushRegistration').mockImplementation((deps) => {
    captured = deps
    return { reconcile, dispose() {} }
  })
  const storage = Object.assign(
    memoryStorage({ 'axon.token': 'synthetic-review-token' }),
    { settled: () => Promise.resolve() },
  )
  const services = createServices(memoryStorage(), memoryStorage(), {
    ...browserPlatform(),
    secureStorage: storage,
    notificationPermission: () => Promise.resolve('granted'),
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
  try {
    expect(captured!.badge()).toBe(true)
    await vi.waitFor(() => expect(reconcile).toHaveBeenCalled())
    const before = reconcile.mock.calls.length

    services.settings.appBadgeEnabled.value = false
    expect(captured!.badge()).toBe(false)
    expect(reconcile.mock.calls.length).toBe(before + 1)

    services.settings.appBadgeEnabled.value = true
    expect(captured!.badge()).toBe(true)
    expect(reconcile.mock.calls.length).toBe(before + 2)
  } finally {
    services.live.stop()
  }
})
