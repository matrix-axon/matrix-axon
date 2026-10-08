import { signal } from '@preact/signals'
import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  appBadgeAvailable,
  applyAppBadge,
  badgeNeedsNotificationPermission,
  notificationPermissionAvailable,
  requestAppBadgeNotificationPermission,
} from './app-badge'
import type { RoomsStore } from './stores/rooms'
import type { SettingsStore } from './stores/settings'

function fakeSettings(appBadgeEnabled: boolean): SettingsStore {
  return {
    appBadgeEnabled: signal(appBadgeEnabled),
  } as unknown as SettingsStore
}

function fakeRooms(unreadTotal: number, loading = false): RoomsStore {
  return {
    unreadTotal: signal(unreadTotal),
    loading: signal(loading),
  } as unknown as RoomsStore
}

/** The effect paints on a microtask, after the turn's last unread write. */
function flushBadge(): Promise<void> {
  return Promise.resolve()
}

describe('appBadgeAvailable', () => {
  afterEach(() => {
    // @ts-expect-error test-only cleanup of properties this suite defines
    delete navigator.setAppBadge
    // @ts-expect-error test-only cleanup of properties this suite defines
    delete navigator.clearAppBadge
  })

  it('reflects whether the Badging API exists on navigator', () => {
    expect(appBadgeAvailable()).toBe(false)
    Object.assign(navigator, {
      setAppBadge: async () => {},
      clearAppBadge: async () => {},
    })
    expect(appBadgeAvailable()).toBe(true)
  })

  it('rejects a declared-but-undefined setAppBadge (#435)', () => {
    // At least one WebKit runtime declares `setAppBadge` on `Navigator`
    // without an implementation behind it, so the key is present while the
    // value is `undefined` and the call throws. Detection has to look at the
    // value.
    Object.assign(navigator, {
      setAppBadge: undefined,
      clearAppBadge: async () => {},
    })
    expect('setAppBadge' in navigator).toBe(true)
    expect(appBadgeAvailable()).toBe(false)
  })

  it('rejects a declared-but-undefined clearAppBadge', () => {
    // Checked independently of `setAppBadge`: a runtime that lies about one
    // says nothing about the other, and the effect calls both.
    Object.assign(navigator, {
      setAppBadge: async () => {},
      clearAppBadge: undefined,
    })
    expect('clearAppBadge' in navigator).toBe(true)
    expect(appBadgeAvailable()).toBe(false)
  })
})

describe('applyAppBadge (ADR 0080)', () => {
  afterEach(() => {
    // @ts-expect-error test-only cleanup of properties this suite defines
    delete navigator.setAppBadge
    // @ts-expect-error test-only cleanup of properties this suite defines
    delete navigator.clearAppBadge
  })

  it('is a no-op when the Badging API is unsupported', () => {
    const dispose = applyAppBadge(fakeSettings(true), fakeRooms(3))
    dispose()
  })

  it('sets the badge to the summed unread-message total while enabled and nonzero', async () => {
    const setAppBadge = vi.fn().mockResolvedValue(undefined)
    const clearAppBadge = vi.fn().mockResolvedValue(undefined)
    Object.assign(navigator, { setAppBadge, clearAppBadge })

    const settings = fakeSettings(true)
    const rooms = fakeRooms(5)
    const dispose = applyAppBadge(settings, rooms)

    await flushBadge()
    expect(setAppBadge).toHaveBeenCalledWith(5)
    expect(clearAppBadge).not.toHaveBeenCalled()
    dispose()
  })

  it('does not clear on launch when nothing has been painted', async () => {
    // A launch at zero, or with the setting already off, used to send a
    // clear. On iOS 15 that also removes delivered notifications.
    const setAppBadge = vi.fn().mockResolvedValue(undefined)
    const clearAppBadge = vi.fn().mockResolvedValue(undefined)
    Object.assign(navigator, { setAppBadge, clearAppBadge })

    const unread = fakeRooms(0)
    const off = fakeSettings(false)
    const disposeUnread = applyAppBadge(fakeSettings(true), unread)
    await flushBadge()
    disposeUnread()

    const disposeOff = applyAppBadge(off, fakeRooms(2))
    await flushBadge()
    expect(setAppBadge).not.toHaveBeenCalled()
    expect(clearAppBadge).not.toHaveBeenCalled()
    disposeOff()
  })

  it('clears the badge when the setting is turned off', async () => {
    const setAppBadge = vi.fn().mockResolvedValue(undefined)
    const clearAppBadge = vi.fn().mockResolvedValue(undefined)
    Object.assign(navigator, { setAppBadge, clearAppBadge })

    const settings = fakeSettings(true)
    const rooms = fakeRooms(2)
    const dispose = applyAppBadge(settings, rooms)

    await flushBadge()
    expect(setAppBadge).toHaveBeenCalledWith(2)
    settings.appBadgeEnabled.value = false
    await flushBadge()
    expect(clearAppBadge).toHaveBeenCalled()
    dispose()
  })

  it('clears the badge when the unread total drops to zero', async () => {
    const setAppBadge = vi.fn().mockResolvedValue(undefined)
    const clearAppBadge = vi.fn().mockResolvedValue(undefined)
    Object.assign(navigator, { setAppBadge, clearAppBadge })

    const settings = fakeSettings(true)
    const rooms = fakeRooms(1)
    const dispose = applyAppBadge(settings, rooms)
    await flushBadge()
    expect(setAppBadge).toHaveBeenCalledWith(1)

    ;(rooms.unreadTotal as unknown as { value: number }).value = 0
    await flushBadge()
    expect(clearAppBadge).toHaveBeenCalled()
    dispose()
  })

  it('stays inert when setAppBadge is declared but undefined (#435)', () => {
    // The regression this guards: `in`-based detection accepted this runtime,
    // then `setAppBadge(3)` threw synchronously — past the `.catch`, which
    // only ever handled a rejected promise — and out of the effect into
    // whichever write to `unreadTotal` had triggered the re-run.
    const clearAppBadge = vi.fn().mockResolvedValue(undefined)
    Object.assign(navigator, { setAppBadge: undefined, clearAppBadge })

    const rooms = fakeRooms(0)
    const dispose = applyAppBadge(fakeSettings(true), rooms)
    expect(() => {
      ;(rooms.unreadTotal as unknown as { value: number }).value = 3
    }).not.toThrow()
    expect(clearAppBadge).not.toHaveBeenCalled()
    dispose()
  })

  it('contains a synchronous throw from a badge call', async () => {
    // Belt to the detection's braces: whatever a runtime does at call time,
    // nothing decorative should escape into the signal write.
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {})
    const setAppBadge = vi.fn(() => {
      throw new Error('badge unavailable')
    })
    const clearAppBadge = vi.fn().mockResolvedValue(undefined)
    Object.assign(navigator, { setAppBadge, clearAppBadge })

    const rooms = fakeRooms(0)
    const dispose = applyAppBadge(fakeSettings(true), rooms)
    expect(() => {
      ;(rooms.unreadTotal as unknown as { value: number }).value = 2
    }).not.toThrow()
    await flushBadge()
    expect(setAppBadge).toHaveBeenCalledWith(2)
    expect(consoleError).toHaveBeenCalled()
    consoleError.mockRestore()
    dispose()
  })

  it('tolerates an implementation that returns no promise', async () => {
    // `.catch` on a bare `undefined` return would itself throw.
    const setAppBadge = vi.fn(() => undefined)
    const clearAppBadge = vi.fn().mockResolvedValue(undefined)
    Object.assign(navigator, { setAppBadge, clearAppBadge })

    let dispose = (): void => {}
    expect(() => {
      dispose = applyAppBadge(fakeSettings(true), fakeRooms(4))
    }).not.toThrow()
    await flushBadge()
    expect(setAppBadge).toHaveBeenCalledWith(4)
    dispose()
  })

  it('grows the badge as further messages arrive in an already-unread room', async () => {
    // Regression: an earlier version of this effect used the *count of
    // unread rooms*, which stayed stuck at 1 while more messages piled up in
    // the same room (reported as "badge appears but isn't increasing").
    const setAppBadge = vi.fn().mockResolvedValue(undefined)
    const clearAppBadge = vi.fn().mockResolvedValue(undefined)
    Object.assign(navigator, { setAppBadge, clearAppBadge })

    const settings = fakeSettings(true)
    const rooms = fakeRooms(1)
    const dispose = applyAppBadge(settings, rooms)
    await flushBadge()
    expect(setAppBadge).toHaveBeenLastCalledWith(1)

    ;(rooms.unreadTotal as unknown as { value: number }).value = 2
    await flushBadge()
    expect(setAppBadge).toHaveBeenLastCalledWith(2)

    ;(rooms.unreadTotal as unknown as { value: number }).value = 3
    await flushBadge()
    expect(setAppBadge).toHaveBeenLastCalledWith(3)
    dispose()
  })
})

describe('applyAppBadge in the shell', () => {
  afterEach(() => {
    // @ts-expect-error test-only cleanup of properties this suite defines
    delete navigator.setAppBadge
    // @ts-expect-error test-only cleanup of properties this suite defines
    delete navigator.clearAppBadge
  })

  it('sets the platform badge and does not call navigator', async () => {
    const setIconBadge = vi.fn().mockResolvedValue(undefined)
    const setAppBadge = vi.fn().mockResolvedValue(undefined)
    const clearAppBadge = vi.fn().mockResolvedValue(undefined)
    Object.assign(navigator, { setAppBadge, clearAppBadge })

    const dispose = applyAppBadge(
      fakeSettings(true),
      fakeRooms(5),
      setIconBadge,
    )

    await flushBadge()
    expect(setIconBadge).toHaveBeenCalledWith(5)
    expect(setAppBadge).not.toHaveBeenCalled()
    expect(clearAppBadge).not.toHaveBeenCalled()
    dispose()
  })

  it('sends one value for a burst of unread totals in the same turn', async () => {
    const setIconBadge = vi.fn().mockResolvedValue(undefined)
    const rooms = fakeRooms(0)
    const dispose = applyAppBadge(fakeSettings(true), rooms, setIconBadge)
    ;(rooms.unreadTotal as unknown as { value: number }).value = 3
    ;(rooms.unreadTotal as unknown as { value: number }).value = 5
    ;(rooms.unreadTotal as unknown as { value: number }).value = 12
    await flushBadge()
    expect(setIconBadge).toHaveBeenCalledTimes(1)
    expect(setIconBadge).toHaveBeenCalledWith(12)
    dispose()
  })

  it('does not clear a settled launch that has nothing unread', async () => {
    const setIconBadge = vi.fn().mockResolvedValue(undefined)
    const rooms = fakeRooms(0)
    const dispose = applyAppBadge(fakeSettings(true), rooms, setIconBadge)
    await flushBadge()
    expect(setIconBadge).not.toHaveBeenCalled()

    ;(rooms.unreadTotal as unknown as { value: number }).value = 4
    await flushBadge()
    expect(setIconBadge).toHaveBeenCalledTimes(1)
    expect(setIconBadge).toHaveBeenCalledWith(4)
    dispose()
  })

  it('retries the same total after a throw', async () => {
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {})
    let fail = true
    const setIconBadge = vi.fn(() => {
      if (fail) {
        throw new Error('badge unavailable')
      }
    })
    const rooms = fakeRooms(4)
    const dispose = applyAppBadge(fakeSettings(true), rooms, setIconBadge)
    await flushBadge()
    expect(setIconBadge).toHaveBeenCalledTimes(1)
    fail = false
    ;(rooms.loading as unknown as { value: boolean }).value = true
    await flushBadge()
    expect(setIconBadge).toHaveBeenCalledTimes(2)
    expect(setIconBadge).toHaveBeenLastCalledWith(4)
    consoleError.mockRestore()
    dispose()
  })

  it('retries the same total after a rejection', async () => {
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {})
    let fail = true
    const setIconBadge = vi.fn(() =>
      fail
        ? Promise.reject(new Error('badge rejected'))
        : Promise.resolve(undefined),
    )
    const rooms = fakeRooms(4)
    const dispose = applyAppBadge(fakeSettings(true), rooms, setIconBadge)
    await vi.waitFor(() => expect(consoleError).toHaveBeenCalledTimes(1))
    fail = false
    ;(rooms.loading as unknown as { value: boolean }).value = true
    await flushBadge()
    expect(setIconBadge).toHaveBeenCalledTimes(2)
    expect(setIconBadge).toHaveBeenLastCalledWith(4)
    consoleError.mockRestore()
    dispose()
  })

  it('keeps a newer total when an older badge call fails', async () => {
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {})
    let rejectFirst: (reason: Error) => void = () => {}
    const first = new Promise<void>((_resolve, reject) => {
      rejectFirst = reject
    })
    let calls = 0
    const setIconBadge = vi.fn(() => {
      calls += 1
      return calls === 1 ? first : Promise.resolve(undefined)
    })
    const rooms = fakeRooms(4)
    const dispose = applyAppBadge(fakeSettings(true), rooms, setIconBadge)
    await flushBadge()
    ;(rooms.unreadTotal as unknown as { value: number }).value = 6
    await flushBadge()
    expect(setIconBadge).toHaveBeenLastCalledWith(6)
    rejectFirst(new Error('stale'))
    await vi.waitFor(() => expect(consoleError).toHaveBeenCalledTimes(1))
    ;(rooms.loading as unknown as { value: boolean }).value = true
    await flushBadge()
    expect(setIconBadge).toHaveBeenCalledTimes(2)
    expect(setIconBadge).toHaveBeenLastCalledWith(6)
    consoleError.mockRestore()
    dispose()
  })

  it('does not clear before the room list has loaded, then clears on sign-out', async () => {
    const setIconBadge = vi.fn().mockResolvedValue(undefined)
    const rooms = fakeRooms(0, true)
    const dispose = applyAppBadge(fakeSettings(true), rooms, setIconBadge)
    await flushBadge()
    expect(setIconBadge).not.toHaveBeenCalled()

    ;(rooms.loading as unknown as { value: boolean }).value = false
    ;(rooms.unreadTotal as unknown as { value: number }).value = 7
    await flushBadge()
    expect(setIconBadge).toHaveBeenCalledTimes(1)
    expect(setIconBadge).toHaveBeenCalledWith(7)

    // Sign-out walks the total to zero and then marks the store loading again.
    // That later zero is a real clear.
    ;(rooms.loading as unknown as { value: boolean }).value = true
    ;(rooms.unreadTotal as unknown as { value: number }).value = 0
    await flushBadge()
    expect(setIconBadge).toHaveBeenLastCalledWith(null)
    dispose()
  })

  it('clears when the setting is off or the total is zero, and grows within one room', async () => {
    const setIconBadge = vi.fn().mockResolvedValue(undefined)
    const settings = fakeSettings(true)
    const rooms = fakeRooms(1)
    const dispose = applyAppBadge(settings, rooms, setIconBadge)

    await flushBadge()
    expect(setIconBadge).toHaveBeenLastCalledWith(1)
    ;(rooms.unreadTotal as unknown as { value: number }).value = 4
    await flushBadge()
    expect(setIconBadge).toHaveBeenLastCalledWith(4)
    ;(rooms.unreadTotal as unknown as { value: number }).value = 0
    await flushBadge()
    expect(setIconBadge).toHaveBeenLastCalledWith(null)

    settings.appBadgeEnabled.value = true
    ;(rooms.unreadTotal as unknown as { value: number }).value = 2
    await flushBadge()
    expect(setIconBadge).toHaveBeenLastCalledWith(2)
    settings.appBadgeEnabled.value = false
    await flushBadge()
    expect(setIconBadge).toHaveBeenLastCalledWith(null)
    // The setting is still off, so another unread write is the same clear.
    ;(rooms.unreadTotal as unknown as { value: number }).value = 8
    await flushBadge()
    expect(setIconBadge).toHaveBeenCalledTimes(5)
    dispose()
  })

  it('contains a throw from the platform badge', async () => {
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {})
    const rooms = fakeRooms(1)
    const dispose = applyAppBadge(fakeSettings(true), rooms, () => {
      throw new Error('badge unavailable')
    })
    await flushBadge()
    expect(consoleError).toHaveBeenCalledTimes(1)
    expect(() => {
      ;(rooms.unreadTotal as unknown as { value: number }).value = 2
    }).not.toThrow()
    await flushBadge()
    expect(consoleError).toHaveBeenCalledTimes(2)
    consoleError.mockRestore()
    dispose()
  })

  it('contains a rejection from the platform badge', async () => {
    const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {})
    const dispose = applyAppBadge(fakeSettings(true), fakeRooms(3), () =>
      Promise.reject(new Error('badge rejected')),
    )
    await vi.waitFor(() => expect(consoleError).toHaveBeenCalledTimes(1))
    consoleError.mockRestore()
    dispose()
  })
})

function stubUserAgent(userAgent: string): void {
  vi.stubGlobal('navigator', { ...navigator, userAgent })
}

describe('badgeNeedsNotificationPermission (ADR 0080)', () => {
  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('is true for real Safari on iOS and macOS', () => {
    stubUserAgent(
      'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1',
    )
    expect(badgeNeedsNotificationPermission()).toBe(true)

    stubUserAgent(
      'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15',
    )
    expect(badgeNeedsNotificationPermission()).toBe(true)
  })

  it('is false for Chromium, Chrome-on-iOS, Firefox-on-iOS, and Android', () => {
    stubUserAgent(
      'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36',
    )
    expect(badgeNeedsNotificationPermission()).toBe(false)

    stubUserAgent(
      'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/120.0 Mobile/15E148 Safari/604.1',
    )
    expect(badgeNeedsNotificationPermission()).toBe(false)

    stubUserAgent(
      'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) FxiOS/120.0 Mobile/15E148 Safari/604.1',
    )
    expect(badgeNeedsNotificationPermission()).toBe(false)

    stubUserAgent(
      'Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36',
    )
    expect(badgeNeedsNotificationPermission()).toBe(false)
  })
})

describe('notificationPermissionAvailable', () => {
  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('reflects whether the Notification API exists', () => {
    expect(notificationPermissionAvailable()).toBe(false)
    vi.stubGlobal('Notification', { permission: 'default' })
    expect(notificationPermissionAvailable()).toBe(true)
  })
})

describe('requestAppBadgeNotificationPermission (ADR 0080)', () => {
  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('returns null when the Notification API is absent', () => {
    expect(requestAppBadgeNotificationPermission()).toBeNull()
  })

  it('returns null without prompting when permission is already decided', () => {
    const requestPermission = vi.fn()
    vi.stubGlobal('Notification', { permission: 'granted', requestPermission })
    expect(requestAppBadgeNotificationPermission()).toBeNull()
    expect(requestPermission).not.toHaveBeenCalled()

    vi.stubGlobal('Notification', { permission: 'denied', requestPermission })
    expect(requestAppBadgeNotificationPermission()).toBeNull()
    expect(requestPermission).not.toHaveBeenCalled()
  })

  it('prompts when permission is undecided', async () => {
    const requestPermission = vi.fn().mockResolvedValue('granted')
    vi.stubGlobal('Notification', { permission: 'default', requestPermission })

    const result = requestAppBadgeNotificationPermission()
    expect(requestPermission).toHaveBeenCalled()
    await expect(result).resolves.toBe('granted')
  })
})
