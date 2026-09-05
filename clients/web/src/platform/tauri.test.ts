import { describe, expect, it, vi } from 'vitest'

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

import { adapt, tauriPlatform } from './tauri'

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

  it("keeps the caller's own signal", async () => {
    // The first-run health probe wants a far shorter bound than the backstop;
    // overriding it would make the setup screen sit there.
    tauriFetch.mockClear()
    const controller = new AbortController()
    await tauriPlatform().fetch('https://axon.example/healthz', {
      signal: controller.signal,
    })

    const init = tauriFetch.mock.calls[0]?.[1]
    expect(init?.signal).toBe(controller.signal)
  })
})
