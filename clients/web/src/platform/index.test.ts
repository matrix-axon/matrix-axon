import { describe, expect, it, vi } from 'vitest'
import { browserPlatform } from './index'

describe('browserPlatform', () => {
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
