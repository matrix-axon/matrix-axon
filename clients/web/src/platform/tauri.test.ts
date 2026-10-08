import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const tauriFetch = vi.fn<
  (input: unknown, init?: RequestInit) => Promise<Response>
>(() => Promise.resolve(new Response('')))
vi.mock('@tauri-apps/plugin-http', () => ({
  fetch: (input: unknown, init?: RequestInit) => tauriFetch(input, init),
}))
vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn() }))
vi.mock('@tauri-apps/plugin-fs', () => ({ writeFile: vi.fn() }))
vi.mock('@tauri-apps/plugin-opener', () => ({ openUrl: vi.fn() }))
vi.mock('@tauri-apps/plugin-websocket', () => ({
  default: { connect: vi.fn() },
}))
vi.mock('@tauri-apps/api/event', () => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}))
vi.mock('@tauri-apps/plugin-deep-link', () => ({
  getCurrent: vi.fn(() => Promise.resolve(null)),
  onOpenUrl: vi.fn(() => Promise.resolve(() => {})),
}))
const createChannel = vi.fn((channel: unknown) =>
  Promise.resolve(channel).then(() => undefined),
)
const removeChannel = vi.fn((id: unknown) =>
  Promise.resolve(id).then(() => undefined),
)
const onAction = vi.fn<(cb: (payload: unknown) => void) => Promise<() => void>>(
  () => Promise.resolve(() => {}),
)
vi.mock('@tauri-apps/plugin-notification', () => ({
  createChannel: (channel: unknown) => createChannel(channel),
  removeChannel: (id: unknown) => removeChannel(id),
  onAction: (cb: (payload: unknown) => void) => onAction(cb),
  Importance: { None: 0, Min: 1, Low: 2, Default: 3, High: 4 },
}))

import { listen } from '@tauri-apps/api/event'
import { getCurrent, onOpenUrl } from '@tauri-apps/plugin-deep-link'
import { save } from '@tauri-apps/plugin-dialog'
import { openUrl } from '@tauri-apps/plugin-opener'
import {
  adapt,
  boundedSignal,
  isMobileShell,
  resetMessageNotificationStartupForTests,
  tauriPlatform,
} from './tauri'

/**
 * A stand-in for the websocket plugin's client. Only `addListener` and
 * `disconnect` are used by the adapter.
 */
function fakeClient() {
  const listeners: ((message: unknown) => void)[] = []
  return {
    client: {
      addListener: (cb: (message: unknown) => void) => {
        listeners.push(cb)
        return () => {}
      },
      disconnect: () => Promise.resolve(),
    },
    emit(message: unknown) {
      for (const l of [...listeners]) {
        l(message)
      }
    },
  }
}

describe('the shell socket adapter', () => {
  it('reports a read failure as error-then-close', async () => {
    // The plugin's Rust side serialises a read failure to a *string* and then
    // ends its read loop without sending `Close`. Ignoring it left the socket
    // reported as live for the rest of the session, with no reconnection.
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    const fake = fakeClient()
    const socket = adapt(Promise.resolve(fake.client as never))
    const events: string[] = []
    socket.onerror = () => events.push('error')
    socket.onclose = () => events.push('close')
    await Promise.resolve()

    fake.emit('IO error: connection reset by peer')

    expect(events).toEqual(['error', 'close'])
    warn.mockRestore()
  })

  it('passes text frames through', async () => {
    const fake = fakeClient()
    const socket = adapt(Promise.resolve(fake.client as never))
    const seen: unknown[] = []
    socket.onmessage = (event) => seen.push(event.data)
    await Promise.resolve()

    fake.emit({ type: 'Text', data: '{"kind":"ping"}' })

    expect(seen).toEqual(['{"kind":"ping"}'])
  })

  it('ignores ping and pong rather than treating them as failures', async () => {
    const fake = fakeClient()
    const socket = adapt(Promise.resolve(fake.client as never))
    const events: string[] = []
    socket.onerror = () => events.push('error')
    socket.onclose = () => events.push('close')
    await Promise.resolve()

    fake.emit({ type: 'Ping', data: [] })
    fake.emit({ type: 'Pong', data: [] })

    expect(events).toEqual([])
  })

  it('closes only once, however the socket ends', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    const fake = fakeClient()
    const socket = adapt(Promise.resolve(fake.client as never))
    let closes = 0
    socket.onclose = () => (closes += 1)
    await Promise.resolve()

    fake.emit('IO error')
    fake.emit({ type: 'Close', data: null })

    expect(closes).toBe(1)
    warn.mockRestore()
  })
})

describe('the shell socket connect bound', () => {
  it('fails a connect that never settles', async () => {
    // The plugin awaits the handshake with no bound of its own, so a host that
    // accepts the TCP connection and then says nothing left the connection
    // stuck at `connecting` for the rest of the session — never failing, and
    // so never triggering `live-connection`'s backoff.
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    vi.useFakeTimers()
    try {
      const socket = adapt(new Promise(() => {}), 20)
      const events: string[] = []
      socket.onerror = () => events.push('error')
      socket.onclose = () => events.push('close')

      expect(events).toEqual([])
      vi.advanceTimersByTime(20)

      expect(events).toEqual(['error', 'close'])
      expect(warn).toHaveBeenCalledWith(
        expect.stringContaining('did not connect'),
      )
    } finally {
      vi.useRealTimers()
      warn.mockRestore()
    }
  })

  it('disconnects a connection that lands after the bound', async () => {
    // Otherwise a slow server resurrects a socket whose caller has already
    // been told it closed, leaving a live read loop nothing is listening to.
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    const fake = fakeClient()
    const disconnect = vi.fn(() => Promise.resolve())
    let land: (client: never) => void = () => {}
    const socket = adapt(
      new Promise<never>((resolve) => {
        land = resolve
      }),
      20,
    )
    const events: string[] = []
    socket.onopen = () => events.push('open')
    socket.onclose = () => events.push('close')
    await new Promise((r) => setTimeout(r, 40))

    land({ ...fake.client, disconnect } as never)
    await Promise.resolve()
    await Promise.resolve()

    expect(disconnect).toHaveBeenCalled()
    expect(events).toEqual(['close'])
    warn.mockRestore()
  })
})

describe('the shell external opener', () => {
  it('rejects when the link could not be opened, naming only the origin', async () => {
    // It used to log and swallow, which suited the link handler and stranded
    // OAuth: handing off to the browser *is* `startSignIn`, so a failure that
    // never reached the caller was a sign-in that silently never began.
    const opener = vi.mocked(openUrl)
    opener.mockClear()
    opener.mockRejectedValue(
      new Error('opener denied https://media.example/x?sig=SECRET'),
    )

    await expect(
      tauriPlatform().openExternal?.(
        'https://media.example/x?sig=SECRET&token=ALSO_SECRET',
      ),
    ).rejects.toThrow('could not open an external link (https://media.example)')
  })

  it('does not carry the query into the failure', async () => {
    // The plugin repeats the URL it was given, and a link in a room can carry
    // a signed media URL or credentials in its query — so the message is
    // rewritten rather than forwarded, or reporting an error writes secrets
    // the user never chose to record.
    const opener = vi.mocked(openUrl)
    opener.mockClear()
    opener.mockRejectedValue(new Error('opener denied https://x/y?sig=SECRET'))

    const failure = await tauriPlatform()
      .openExternal?.('https://x/y?sig=SECRET')
      .catch((err: unknown) => (err as Error).message)

    expect(failure).not.toContain('SECRET')
  })

  it('resolves when the link opened', async () => {
    const opener = vi.mocked(openUrl)
    opener.mockClear()
    opener.mockResolvedValue(undefined)

    await expect(
      tauriPlatform().openExternal?.('https://example.org/docs'),
    ).resolves.toBeUndefined()
  })
})

describe('the shell save dialog', () => {
  it('offers a basename, never a path the sender chose', async () => {
    // `filename` is `content.filename`/`content.body` off the event, so a room
    // can put anything in it. `<a download>` dropped the directory; this must
    // not lose that in the port to a native dialog.
    const saved = vi.mocked(save)
    saved.mockClear()
    saved.mockResolvedValue(null)

    await tauriPlatform().saveFile({
      blob: new Blob(['x']),
      filename: '../../.config/autostart/evil.desktop',
      mimetype: 'text/plain',
    })
    expect(saved.mock.calls[0]?.[0]).toEqual({
      defaultPath: 'evil.desktop',
    })

    await tauriPlatform().saveFile({
      blob: new Blob(['x']),
      filename: 'C:\\Windows\\System32\\drivers\\etc\\hosts',
      mimetype: 'text/plain',
    })
    expect(saved.mock.calls[1]?.[0]).toEqual({ defaultPath: 'hosts' })

    // A name that is only a path leaves the dialog something to show.
    await tauriPlatform().saveFile({
      blob: new Blob(['x']),
      filename: '../',
      mimetype: 'text/plain',
    })
    expect(saved.mock.calls[2]?.[0]).toEqual({ defaultPath: 'download' })
  })
})

describe('the shell request bound', () => {
  it('abandons a request the caller left unbounded', async () => {
    // `reqwest` behind the http plugin applies no timeout, so a server that
    // accepts the connection and never answers held one of `media-service`'s
    // bounded permits for the life of the session.
    tauriFetch.mockClear()
    await tauriPlatform().fetch('https://axon.example/v1/accounts')

    const init = tauriFetch.mock.calls[0]?.[1]
    expect(init?.signal).toBeInstanceOf(AbortSignal)
  })

  it('bounds a request that arrives as a Request object', async () => {
    // The regression this whole helper exists for. `openapi-fetch` builds a
    // `Request` and hands it to the injected fetch, and a `Request` always
    // exposes a signal even when nobody asked for one -- so treating a
    // non-null `input.signal` as the caller's own bound silently exempted
    // every /v1 call the API client makes.
    tauriFetch.mockClear()
    await tauriPlatform().fetch(new Request('https://axon.example/v1/rooms'))

    const init = tauriFetch.mock.calls[0]?.[1]
    expect(init?.signal).toBeInstanceOf(AbortSignal)
    // In `init`, because the plugin reads `init?.signal` and never looks at
    // the request's own -- a bound left on the `Request` would not be honoured.
    expect(init?.signal).not.toBe(
      (tauriFetch.mock.calls[0]?.[0] as Request).signal,
    )
  })

  it('abandons a Request the plugin never answers', async () => {
    const signal = boundedSignal(
      new Request('https://axon.example/v1/rooms'),
      undefined,
      1,
    )
    await expect(aborted(signal)).resolves.toBe('TimeoutError')
  })

  it("keeps the caller's own, shorter bound", async () => {
    // The first-run health probe wants a far shorter bound than the backstop;
    // composing rather than replacing means whichever fires first wins, so the
    // probe still gives up in seconds and the setup screen does not sit there.
    const controller = new AbortController()
    const signal = boundedSignal(
      'https://axon.example/healthz',
      { signal: controller.signal },
      60_000,
    )
    controller.abort(new DOMException('probe gave up', 'AbortError'))
    await expect(aborted(signal)).resolves.toBe('AbortError')
  })
})

/** The name of the reason a signal aborts with, once it does. */
function aborted(signal: AbortSignal): Promise<string> {
  return new Promise((resolve) => {
    if (signal.aborted) {
      resolve((signal.reason as DOMException).name)
      return
    }
    signal.addEventListener('abort', () => {
      resolve((signal.reason as DOMException).name)
    })
  })
}

describe('deep-link delivery', () => {
  it('delivers a cold-launch URL once, not once per channel', async () => {
    // The two channels are not exclusive. On a cold launch the plugin sets
    // what `getCurrent` returns *and* emits the event `onOpenUrl` listens for
    // — `handle_cli_arguments` does both on Windows and Linux,
    // `RunEvent::Opened` does both on macOS. Whether the emit beats the
    // webview's subscription is timing, not design.
    //
    // A repeat is not harmless: `completeRedirect` consumes the pending PKCE
    // entry, so re-delivering a callback that just worked fails its state
    // check and paints an error over a sign-in that succeeded.
    const url = 'org.matrixaxon.axon:/oauth/callback?code=c&state=s'
    let emit: ((urls: string[]) => void) | null = null
    vi.mocked(getCurrent).mockResolvedValue([url])
    vi.mocked(onOpenUrl).mockImplementation((handler) => {
      emit = handler
      return Promise.resolve(() => {})
    })

    const seen: string[] = []
    tauriPlatform().onDeepLink?.((u) => seen.push(u.toString()))
    await vi.waitFor(() => expect(seen).toHaveLength(1))

    // The same URL arriving again through the other channel is the repeat.
    emit!([url])
    expect(seen).toHaveLength(1)
  })

  it('still delivers a genuinely different link', async () => {
    // De-duplication must not swallow a second, real callback — a user who
    // cancels and signs in again gets two distinct URLs.
    let emit: ((urls: string[]) => void) | null = null
    vi.mocked(getCurrent).mockResolvedValue(null)
    vi.mocked(onOpenUrl).mockImplementation((handler) => {
      emit = handler
      return Promise.resolve(() => {})
    })

    const seen: string[] = []
    tauriPlatform().onDeepLink?.((u) => seen.push(u.toString()))
    await vi.waitFor(() => expect(emit).not.toBeNull())

    emit!(['org.matrixaxon.axon:/oauth/callback?code=one&state=a'])
    emit!(['org.matrixaxon.axon:/oauth/callback?code=two&state=b'])

    expect(seen).toHaveLength(2)
  })
})

describe('page zoom (ADR 0107)', () => {
  it('is offered on the desktop and withheld from a phone or tablet', () => {
    expect(isMobileShell('Mozilla/5.0 (Windows NT 10.0; Win64; x64)', 0)).toBe(
      false,
    )
    expect(
      isMobileShell('Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)', 0),
    ).toBe(false)
    expect(isMobileShell('Mozilla/5.0 (X11; Linux x86_64)', 0)).toBe(false)
    expect(isMobileShell('Mozilla/5.0 (iPhone; CPU iPhone OS 18_0)', 5)).toBe(
      true,
    )
    expect(isMobileShell('Mozilla/5.0 (Linux; Android 15)', 5)).toBe(true)
    // An iPad's webview can claim to be a Mac; the touch screen gives it away.
    expect(
      isMobileShell('Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)', 5),
    ).toBe(true)
  })

  it('wires setZoom only where the shell is not mobile', () => {
    vi.spyOn(navigator, 'userAgent', 'get').mockReturnValue(
      'Mozilla/5.0 (Windows NT 10.0; Win64; x64)',
    )
    expect(tauriPlatform().setZoom).toBeTypeOf('function')
    vi.spyOn(navigator, 'userAgent', 'get').mockReturnValue(
      'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0)',
    )
    expect(tauriPlatform().setZoom).toBeNull()
    vi.restoreAllMocks()
  })
})

describe('native menu commands (ADR 0107)', () => {
  it('passes on our commands and drops anything else', async () => {
    const handler = vi.fn()
    const unsubscribe = tauriPlatform().onMenuCommand!(handler)
    const [event, callback] = vi.mocked(listen).mock.calls.at(-1)!
    expect(event).toBe('axon://menu')

    const deliver = callback as (event: { payload: string }) => void
    deliver({ payload: 'help' })
    deliver({ payload: 'privacy' })
    deliver({ payload: 'zoom-in' })
    deliver({ payload: 'zoom-out' })
    deliver({ payload: 'zoom-reset' })
    deliver({ payload: 'quit' })
    expect(handler.mock.calls).toEqual([
      ['help'],
      ['privacy'],
      ['zoom-in'],
      ['zoom-out'],
      ['zoom-reset'],
    ])
    unsubscribe()
  })
})

describe('message notifications in the shell', () => {
  const shell = () => window as unknown as Record<string, unknown>
  const ANDROID =
    'Mozilla/5.0 (Linux; Android 14; sdk_gphone64_x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36'

  beforeEach(() => {
    resetMessageNotificationStartupForTests()
    createChannel.mockClear()
    removeChannel.mockClear()
    onAction.mockClear()
  })

  afterEach(() => {
    delete shell().__TAURI_INTERNALS__
    vi.unstubAllGlobals()
    // Node's experimental localStorage stub has no Storage methods. The posts
    // under test already swallow a store that cannot be written.
    if (typeof localStorage?.removeItem === 'function') {
      localStorage.removeItem('axon.notification-targets')
    }
  })

  function asAndroid(): void {
    vi.stubGlobal('navigator', { userAgent: ANDROID, maxTouchPoints: 1 })
  }

  function installInvoke(
    invoke: (cmd: string, args?: unknown) => Promise<unknown>,
  ) {
    shell().__TAURI_INTERNALS__ = {
      invoke,
      transformCallback: () => 0,
    }
  }

  it('opens a high-importance messages channel on Android', async () => {
    asAndroid()
    tauriPlatform()
    await vi.waitFor(() => expect(createChannel).toHaveBeenCalled())
    expect(removeChannel).toHaveBeenCalledWith('messages')
    expect(createChannel).toHaveBeenCalledWith({
      id: 'messages-v2',
      name: 'Messages',
      description: 'New messages',
      importance: 4,
    })
    expect(onAction).toHaveBeenCalledTimes(1)
    tauriPlatform()
    expect(onAction).toHaveBeenCalledTimes(1)
    expect(createChannel).toHaveBeenCalledTimes(1)
  })

  it('does not register a desktop tap listener or an Android channel', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    tauriPlatform()
    await Promise.resolve()
    expect(onAction).not.toHaveBeenCalled()
    expect(createChannel).not.toHaveBeenCalled()
    expect(removeChannel).not.toHaveBeenCalled()
    expect(warn).not.toHaveBeenCalled()
    warn.mockRestore()
  })

  it('reports an Android channel failure instead of swallowing it', async () => {
    asAndroid()
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    createChannel.mockRejectedValueOnce(
      new Error('notification.create_channel not allowed'),
    )
    tauriPlatform()
    await vi.waitFor(() => expect(warn).toHaveBeenCalled())
    expect(
      warn.mock.calls.some((call) => String(call[1]).includes('not allowed')),
    ).toBe(true)
    warn.mockRestore()
  })

  it('posts through the plugin command, not window.Notification', async () => {
    const invoke = vi.fn((...args: unknown[]) =>
      Promise.resolve(args[0] ?? null),
    )
    installInvoke(invoke)
    const platform = tauriPlatform()
    await platform.notify({
      title: 'Ops',
      body: '@alice: hello',
      accountId: 'acct',
      roomId: '!room:server',
      eventId: '$evt',
      threadRootId: '$root',
    })
    expect(invoke).toHaveBeenCalledWith(
      'plugin:notification|notify',
      expect.objectContaining({
        options: expect.objectContaining({
          title: 'Ops',
          body: '@alice: hello',
          channelId: 'messages-v2',
          extra: {
            accountId: 'acct',
            roomId: '!room:server',
            eventId: '$evt',
            threadRootId: '$root',
          },
          autoCancel: true,
        }),
      }),
      undefined,
    )
    const posted = invoke.mock.calls[0]?.[1] as
      { options: { id: number } } | undefined
    expect(posted?.options.id).toBeGreaterThan(0)
  })

  it('asks the plugin directly for permission', async () => {
    const invoke = vi.fn((cmd: string) =>
      Promise.resolve(cmd.endsWith('request_permission') ? 'prompt' : null),
    )
    installInvoke(invoke)
    const platform = tauriPlatform()
    await expect(platform.notificationPermission()).resolves.toBe('default')
    await expect(platform.notificationPermission()).resolves.toBe('default')
    expect(
      invoke.mock.calls.filter((call) =>
        String(call[0]).endsWith('is_permission_granted'),
      ),
    ).toHaveLength(1)
    await expect(platform.requestNotificationPermission()).resolves.toBe(
      'default',
    )
    await expect(platform.notificationPermission()).resolves.toBe('default')
    expect(
      invoke.mock.calls.filter((call) =>
        String(call[0]).endsWith('is_permission_granted'),
      ),
    ).toHaveLength(1)
    expect(invoke).toHaveBeenCalledWith(
      'plugin:notification|is_permission_granted',
      {},
      undefined,
    )
    expect(invoke).toHaveBeenCalledWith(
      'plugin:notification|request_permission',
      {},
      undefined,
    )
  })

  it('drops a rejected permission request without an unhandled rejection', async () => {
    const invoke = vi.fn((cmd: string) =>
      String(cmd).endsWith('request_permission')
        ? Promise.reject(new Error('no sheet'))
        : Promise.resolve(null),
    )
    installInvoke(invoke)
    const platform = tauriPlatform()
    await expect(platform.requestNotificationPermission()).rejects.toThrow(
      'no sheet',
    )
    await Promise.resolve()
    await expect(platform.notificationPermission()).resolves.toBe('default')
    expect(
      invoke.mock.calls.filter((call) =>
        String(call[0]).endsWith('is_permission_granted'),
      ),
    ).toHaveLength(1)
  })

  it('keeps a newer permission read when an older request rejects', async () => {
    let rejectFirst: ((error: Error) => void) | undefined
    const invoke = vi.fn((cmd: string) => {
      if (!String(cmd).endsWith('request_permission')) {
        return Promise.resolve(null)
      }
      if (rejectFirst === undefined) {
        return new Promise<string>((_resolve, reject) => {
          rejectFirst = reject
        })
      }
      return Promise.resolve('granted')
    })
    installInvoke(invoke)
    const platform = tauriPlatform()
    const first = platform.requestNotificationPermission()
    await expect(platform.requestNotificationPermission()).resolves.toBe(
      'granted',
    )
    rejectFirst?.(new Error('late'))
    await expect(first).rejects.toThrow('late')
    await Promise.resolve()
    await expect(platform.notificationPermission()).resolves.toBe('granted')
    expect(
      invoke.mock.calls.filter((call) =>
        String(call[0]).endsWith('is_permission_granted'),
      ),
    ).toHaveLength(0)
  })

  it('routes an Android tap and ignores a dismiss', () => {
    asAndroid()
    let listener: ((payload: unknown) => void) | undefined
    onAction.mockImplementation((cb) => {
      listener = cb
      return Promise.resolve(() => {})
    })
    const platform = tauriPlatform()
    const seen: {
      accountId: string
      roomId: string
      eventId: string | null
      threadRootId: string | null
    }[] = []
    const unsubscribe = platform.onNotificationClick?.((click) => {
      seen.push(click)
    })
    try {
      listener?.({
        actionId: 'tap',
        notification: {
          extra: {
            accountId: 'acct',
            roomId: '!r:s',
            eventId: '$evt',
            threadRootId: '$root',
          },
        },
      })
      listener?.({
        actionId: 'dismiss',
        notification: { extra: { accountId: 'acct', roomId: '!r:s' } },
      })
      expect(seen).toEqual([
        {
          accountId: 'acct',
          roomId: '!r:s',
          eventId: '$evt',
          threadRootId: '$root',
        },
      ])
    } finally {
      unsubscribe?.()
      onAction.mockImplementation(() => Promise.resolve(() => {}))
    }
  })
})
