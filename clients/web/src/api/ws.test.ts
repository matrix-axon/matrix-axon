import { describe, expect, it, vi } from 'vitest'
import { apiBaseUrl } from '../services'
import type { LiveSocket } from '../platform'
import { openLiveSocket, wsAuthProtocols, wsUrl } from './ws'

describe('wsUrl', () => {
  it('maps http to ws and https to wss', () => {
    expect(wsUrl('http://localhost:8080')).toBe('ws://localhost:8080/v1/ws')
    expect(wsUrl('https://axon.example.com')).toBe(
      'wss://axon.example.com/v1/ws',
    )
  })

  it('passes explicit ws(s) bases through', () => {
    expect(wsUrl('ws://localhost:8080')).toBe('ws://localhost:8080/v1/ws')
    expect(wsUrl('wss://axon.example.com')).toBe('wss://axon.example.com/v1/ws')
  })

  it('keeps a path prefix, because the base may live under one', () => {
    // This asserted the opposite until review pointed out the contradiction:
    // `normalizeServerUrl` promises `/axon` is preserved and the API client
    // and media service honour that, so discarding it here made one setting
    // mean two things. `server-url.ts`'s `apiUrl` is now the single join.
    expect(wsUrl('https://axon.example.com/axon')).toBe(
      'wss://axon.example.com/axon/v1/ws',
    )
  })

  it('defaults to the page origin (the dev-proxy setup)', () => {
    // jsdom serves the tests from http://localhost:3000 by default.
    expect(wsUrl()).toBe(
      new URL('/v1/ws', window.location.origin)
        .toString()
        .replace(/^http/, 'ws'),
    )
  })

  const sameOrigin = new URL('/v1/ws', window.location.origin)
    .toString()
    .replace(/^http/, 'ws')

  it('resolves a relative base against the page origin', () => {
    // The regression: `apiBaseUrl()` returns '/' for the same-origin default,
    // and `new URL(path, '/')` throws — a parameter default only covers
    // `undefined`, so the explicit '/' slipped straight past it.
    expect(wsUrl('/')).toBe(sameOrigin)
  })

  it('matches the value the live socket is actually opened with', () => {
    // Guards the real call site, `openLiveSocket(token, apiBaseUrl())`. The
    // suite previously only exercised `wsUrl()` with no argument, which is not
    // how anything calls it — that is how the throw shipped.
    expect(wsUrl(apiBaseUrl())).toBe(sameOrigin)
  })

  it('throws on a base whose scheme is not http(s)/ws(s)', () => {
    // The packaged-build bug this guard exists for (ADR 0102 § "What actually
    // blocks a packaged build"). The old code coerced anything unrecognised to
    // `ws:`, which failed two ways and reported neither: against
    // `tauri://localhost` the coercion is a no-op (the URL spec ignores
    // `.protocol` on a non-special scheme) so the socket constructor threw on
    // the scheme, and against `http://tauri.localhost` it produced a valid
    // `ws://tauri.localhost/v1/ws` aimed at the app's own origin. Both end as
    // a permanent "reconnecting" with nothing to diagnose.
    expect(() => wsUrl('tauri://localhost')).toThrow(/absolute http/)
    expect(() => wsUrl('file:///opt/axon/index.html')).toThrow(/absolute http/)
  })

  it('names the offending scheme, since the failure is a config mistake', () => {
    expect(() => wsUrl('tauri://localhost')).toThrow(/tauri:/)
  })

  it('never throws for any base the app can supply', () => {
    // A throw here is invisible in the UI: `socketFactory` throwing sends
    // `openSocket` straight to `scheduleReconnect`, which throws again on
    // every timer, so the client reconnects forever without ever constructing
    // a WebSocket.
    for (const base of ['/', '', undefined, window.location.origin]) {
      expect(() => wsUrl(base)).not.toThrow()
    }
  })
})

describe('wsAuthProtocols', () => {
  it('offers benign axon first, then the bearer.<token> entry (ADR 0029, #238)', () => {
    expect(wsAuthProtocols('tok-123')).toEqual(['axon', 'bearer.tok-123'])
  })

  it('lists axon before the credential so the server echoes a non-secret protocol', () => {
    const protocols = wsAuthProtocols('tok-123')
    expect(protocols[0]).toBe('axon')
    expect(protocols[1]).toContain('tok-123')
  })
})

describe('openLiveSocket', () => {
  const socket = (): LiveSocket => ({
    onopen: null,
    onmessage: null,
    onclose: null,
    onerror: null,
    close: () => {},
  })

  it('hands the platform a URL and the token, not an encoded credential', () => {
    // jsdom has no WebSocket, so the default browser platform cannot be
    // exercised here — which is the point of the seam: the shell substitutes
    // its own transport the same way this test does (ADR 0102 § 2).
    //
    // The assertion is about the *shape* of the contract. A shell opens the
    // socket outside the webview and can send `Authorization`, which is the
    // branch the server tries first; passing `['axon', 'bearer.…']` here would
    // oblige it to parse the browser's workaround back apart.
    const openSocket = vi.fn(socket)
    openLiveSocket('tok-123', 'https://axon.example.com', { openSocket })

    expect(openSocket).toHaveBeenCalledWith(
      'wss://axon.example.com/v1/ws',
      'tok-123',
    )
  })

  it('returns whatever the platform opened', () => {
    const opened = socket()
    const result = openLiveSocket('tok', 'https://axon.example.com', {
      openSocket: () => opened,
    })
    expect(result).toBe(opened)
  })
})
