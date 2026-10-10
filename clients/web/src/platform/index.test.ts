import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  assertBakedShellTarget,
  browserPlatform,
  iconBadgeSupportFor,
  needsCameraCaptureButtons,
} from './index'

describe('browserPlatform', () => {
  it('leaves the icon badge to the Badging API', () => {
    expect(browserPlatform().iconBadgeSupport).toBe('web')
    expect(browserPlatform().setIconBadge).toBeNull()
  })

  it('calls the global fetch, with the right receiver', async () => {
    // An unbound `globalThis.fetch` reference throws "Illegal invocation" in a
    // browser once it is passed around as a value — which is exactly what this
    // seam does with it (into `openapi-fetch`, into the media service).
    const spy = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response('{}'))
    const { fetch } = browserPlatform()

    await expect(
      fetch('https://axon.example.com/v1/rooms'),
    ).resolves.toBeInstanceOf(Response)
    expect(spy).toHaveBeenCalledWith('https://axon.example.com/v1/rooms')

    spy.mockRestore()
  })

  it('resolves the global at call time, not at construction', async () => {
    // The seam must not capture `globalThis.fetch` eagerly. The code it
    // replaced read the global on every call, and msw installs its interceptor
    // by swapping `globalThis.fetch` — so a service graph built before
    // `server.listen()` would hold the *unintercepted* function and quietly
    // make real network requests. Nothing does that ordering today; this keeps
    // it from becoming possible.
    const { fetch } = browserPlatform()

    const late = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response('late'))
    await fetch('https://axon.example.com/v1/rooms')

    expect(late).toHaveBeenCalledOnce()
    late.mockRestore()
  })

  it('encodes the token as subprotocols, because a browser cannot send a header', () => {
    // jsdom has no WebSocket constructor, so stand one in. The seam takes a
    // *token*; turning it into `Sec-WebSocket-Protocol` entries is this
    // platform's business (ADR 0029, #238), and the bearer entry is positional.
    const ctor = vi.fn()
    const original = (globalThis as { WebSocket?: unknown }).WebSocket
    ;(globalThis as { WebSocket?: unknown }).WebSocket = ctor

    browserPlatform().openSocket('wss://axon.example.com/v1/ws', 'tok')

    expect(ctor).toHaveBeenCalledWith('wss://axon.example.com/v1/ws', [
      'axon',
      'bearer.tok',
    ])
    ;(globalThis as { WebSocket?: unknown }).WebSocket = original
  })
})

describe('browserPlatform saving a file', () => {
  const blob = () => new Blob(['bytes'], { type: 'image/png' })

  it('uses a transient anchor when no share sheet takes files', async () => {
    // The sanctioned path: `window.open` is banned repo-wide, so there is no
    // "open it in a tab and save from there" fallback behind this.
    const clicks: HTMLAnchorElement[] = []
    const original = HTMLAnchorElement.prototype.click
    HTMLAnchorElement.prototype.click = function () {
      clicks.push(this as HTMLAnchorElement)
    }
    try {
      const outcome = await browserPlatform().saveFile({
        blob: blob(),
        filename: 'cat.png',
        mimetype: 'image/png',
      })
      expect(outcome).toBe('saved')
      expect(clicks).toHaveLength(1)
      expect(clicks[0].download).toBe('cat.png')
    } finally {
      HTMLAnchorElement.prototype.click = original
    }
  })

  it('offers the share sheet first where one takes files', async () => {
    const share = vi.fn(() => Promise.resolve())
    const nav = navigator as unknown as Record<string, unknown>
    nav.share = share
    nav.canShare = () => true
    try {
      const outcome = await browserPlatform().saveFile({
        blob: blob(),
        filename: 'cat.png',
        mimetype: 'image/png',
      })
      // On a phone the anchor lands the file in Files rather than Photos,
      // which reads as the save having failed.
      expect(outcome).toBe('shared')
      expect(share).toHaveBeenCalled()
    } finally {
      delete nav.share
      delete nav.canShare
    }
  })

  it('reports a dismissed share sheet as cancelled, not failed', async () => {
    const nav = navigator as unknown as Record<string, unknown>
    nav.share = () =>
      Promise.reject(new DOMException('dismissed', 'AbortError'))
    nav.canShare = () => true
    try {
      // Someone who changed their mind must not be shown an error.
      await expect(
        browserPlatform().saveFile({ blob: blob(), filename: 'cat.png' }),
      ).resolves.toBe('cancelled')
    } finally {
      delete nav.share
      delete nav.canShare
    }
  })

  it('leaves external links to the anchor', () => {
    // `null` is the statement that the browser default is already correct.
    // Anything else would break middle-click and modifier-click.
    expect(browserPlatform().openExternal).toBeNull()
  })
})

describe('browser message notifications', () => {
  const original = globalThis.Notification

  afterEach(() => {
    if (original === undefined) {
      delete (globalThis as { Notification?: unknown }).Notification
    } else {
      globalThis.Notification = original
    }
    vi.restoreAllMocks()
  })

  it('tags a toast with the event id and routes the click', () => {
    const constructed: { title: string; options: NotificationOptions }[] = []
    const shown: FakeNotification[] = []
    class FakeNotification {
      static permission: NotificationPermission = 'granted'
      static requestPermission(): Promise<NotificationPermission> {
        return Promise.resolve('granted')
      }
      onclick: (() => void) | null = null
      constructor(title: string, options?: NotificationOptions) {
        constructed.push({ title, options: options ?? {} })
        shown.push(this)
      }
      close(): void {}
    }
    globalThis.Notification = FakeNotification as unknown as typeof Notification
    const platform = browserPlatform()
    const seen: {
      accountId: string
      roomId: string
      eventId: string | null
      threadRootId: string | null
    }[] = []
    const stop = platform.onNotificationClick?.((click) => {
      seen.push(click)
    })
    return platform
      .notify({
        title: 'Ops',
        body: 'Ada: hi',
        accountId: 'acct',
        roomId: '!room:server',
        eventId: '$evt',
        threadRootId: '$root',
      })
      .then(() => {
        expect(constructed).toEqual([
          {
            title: 'Ops',
            options: { body: 'Ada: hi', tag: '$evt' },
          },
        ])
        shown[0]?.onclick?.()
        expect(seen).toEqual([
          {
            accountId: 'acct',
            roomId: '!room:server',
            eventId: '$evt',
            threadRootId: '$root',
          },
        ])
        stop?.()
      })
  })

  it('reports a missing Notification API as unsupported, not denied', async () => {
    delete (globalThis as { Notification?: unknown }).Notification
    const platform = browserPlatform()
    await expect(platform.notificationPermission()).resolves.toBe('unsupported')
    await expect(platform.requestNotificationPermission()).resolves.toBe(
      'unsupported',
    )
  })

  it('warns once when the constructor is illegal, then stays quiet', async () => {
    class IllegalNotification {
      static permission: NotificationPermission = 'default'
      static requestPermission(): Promise<NotificationPermission> {
        return Promise.resolve('granted')
      }
      constructor() {
        throw new TypeError(
          "Failed to construct 'Notification': Illegal constructor",
        )
      }
    }
    globalThis.Notification =
      IllegalNotification as unknown as typeof Notification
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    const platform = browserPlatform()
    await expect(platform.notificationPermission()).resolves.toBe('unsupported')
    IllegalNotification.permission = 'granted'
    // A granted page cannot be probed without showing a toast. The first post
    // is what learns the constructor throws.
    const granted = browserPlatform()
    await granted.notify({
      title: 'Ops',
      body: 'hi',
      accountId: 'acct',
      roomId: '!room:server',
      eventId: '$a',
      threadRootId: null,
    })
    await granted.notify({
      title: 'Ops',
      body: 'hi',
      accountId: 'acct',
      roomId: '!room:server',
      eventId: '$b',
      threadRootId: null,
    })
    expect(warn).toHaveBeenCalledTimes(1)
    warn.mockRestore()
  })
})

describe('iconBadgeSupportFor', () => {
  it('names each shell target the Tauri CLI reports', () => {
    expect(iconBadgeSupportFor('android')).toBe('none')
    // armv7-linux-androideabi keeps `androideabi` as the OS field.
    expect(iconBadgeSupportFor('androideabi')).toBe('none')
    expect(iconBadgeSupportFor('linux')).toBe('launcher-dependent')
    expect(iconBadgeSupportFor('ios')).toBe('permission')
    expect(iconBadgeSupportFor('darwin')).toBe('native')
    expect(iconBadgeSupportFor('windows')).toBe('native')
    // A browser build and vitest do not set the variable. A running shell
    // throws before it uses this answer.
    expect(iconBadgeSupportFor('')).toBe('native')
    // `macos` is the old CLI alias. It is not a target this map claims.
    expect(() => iconBadgeSupportFor('macos')).toThrow(/TAURI_ENV_PLATFORM/)
  })
})

describe('assertBakedShellTarget', () => {
  it('refuses an empty target and accepts a named one', () => {
    expect(() => assertBakedShellTarget('')).toThrow(/TAURI_ENV_PLATFORM/)
    expect(() => assertBakedShellTarget('android')).not.toThrow()
  })
})

describe('needsCameraCaptureButtons', () => {
  const ANDROID_WEBVIEW =
    'Mozilla/5.0 (Linux; Android 13; SM-G781U1 Build/TP1A.220624.014; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/155.0.8059.30 Mobile Safari/537.36'
  const IOS_WEBVIEW =
    'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148'
  const ANDROID_CHROME =
    'Mozilla/5.0 (Linux; Android 13; SM-G781U1) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Mobile Safari/537.36'

  function as(userAgent: string, tauri: boolean): void {
    vi.stubGlobal('navigator', { userAgent })
    if (tauri) {
      vi.stubGlobal('__TAURI_INTERNALS__', {})
      ;(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {}
    }
  }

  afterEach(() => {
    delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__
    vi.unstubAllGlobals()
  })

  it('is true in the packaged Android app', () => {
    as(ANDROID_WEBVIEW, true)
    expect(needsCameraCaptureButtons()).toBe(true)
  })

  it('is false on iOS, whose chooser already has a camera entry', () => {
    as(IOS_WEBVIEW, true)
    expect(needsCameraCaptureButtons()).toBe(false)
  })

  it('is false in Chrome on Android, whose own chooser adds a camera', () => {
    as(ANDROID_CHROME, false)
    expect(needsCameraCaptureButtons()).toBe(false)
  })

  it('is false on desktop', () => {
    as(
      'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/155.0.0.0',
      true,
    )
    expect(needsCameraCaptureButtons()).toBe(false)
  })
})
