import { cleanup, render, screen } from '@testing-library/preact'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { AppRoot } from './app-root'
import { browserPlatform, type Platform } from './platform'
import { resolveApiBaseUrl } from './services'
import { SERVER_URL_KEY } from './server-url'
import type { LiveSocket } from './platform'
import { memoryStorage } from './test/memory-storage'
import { testServices } from './test/services'

afterEach(cleanup)

/** A packaged build: no same-origin API to fall back on. */
const shellPlatform = (): Platform => ({
  ...browserPlatform(),
  defaultApiBaseUrl: null,
})

describe('AppRoot', () => {
  it('asks for a server when the platform has no same-origin default', () => {
    render(<AppRoot platform={shellPlatform()} storage={memoryStorage()} />)
    expect(screen.getByLabelText('Server address')).toBeTruthy()
  })

  it('does not ask once a server has been stored', () => {
    const storage = memoryStorage()
    storage.setItem(SERVER_URL_KEY, 'https://axon.example.com')
    render(
      <AppRoot
        platform={shellPlatform()}
        storage={storage}
        services={testServices()}
      />,
    )
    expect(screen.queryByLabelText('Server address')).toBeNull()
  })

  it('never asks in a browser, whose default is same-origin', () => {
    // The regression that would matter most: a gate in front of every existing
    // browser deployment, where the server is not a choice anyone made. The
    // fallback chain has to bottom out at '/' there, so `AppRoot` resolves a
    // base on the first pass and renders the app exactly as `main.tsx` used to.
    expect(resolveApiBaseUrl(memoryStorage(), browserPlatform())).toBe('/')
  })

  it('prefers a stored server over the platform default', () => {
    const storage = memoryStorage()
    storage.setItem(SERVER_URL_KEY, 'https://axon.example.com')
    expect(resolveApiBaseUrl(storage, browserPlatform())).toBe(
      'https://axon.example.com',
    )
  })

  it('ignores a stored value that is not a usable base', () => {
    const storage = memoryStorage()
    storage.setItem(SERVER_URL_KEY, 'javascript:alert(1)')
    expect(resolveApiBaseUrl(storage, shellPlatform())).toBeNull()
  })

  it('skips the gate when a service graph is injected', () => {
    // Every existing component test supplies its own graph; making them all
    // click through a setup screen would be pure ceremony.
    render(
      <AppRoot
        platform={shellPlatform()}
        storage={memoryStorage()}
        services={testServices()}
      />,
    )
    expect(screen.queryByLabelText('Server address')).toBeNull()
  })
})

describe('AppRoot wiring the transport into the app', () => {
  /**
   * The regression this exists for. `AppRoot` used the platform for the setup
   * screen and for resolving the base URL, then rendered `<App services={…} />`
   * and dropped it — so `createServices()` fell back to `browserPlatform()` and
   * every `/v1` call went out on the *webview's* own fetch.
   *
   * In a browser that is invisible, because the fallback is the right answer
   * there. In a packaged build it is cross-origin against a server that sends
   * no CORS headers, so the room list rejects and the socket never opens: a
   * "Load failed" banner and a permanent "Reconnecting…", with nothing naming
   * the cause. Neither the type checker nor any existing test objected, because
   * a dropped optional prop is not an error anywhere.
   */
  it('builds the service graph on the platform it was given', async () => {
    const storage = memoryStorage()
    storage.setItem(SERVER_URL_KEY, 'https://axon.example.com')

    const socket = (): LiveSocket => ({
      onopen: null,
      onmessage: null,
      onclose: null,
      onerror: null,
      close: () => {},
    })
    // Answer everything with the shared error envelope. What is asserted is
    // *which transport* the request went out on, not what came back, and a
    // uniform 503 keeps every store on its error path rather than handing
    // components a body shaped for some other endpoint.
    const fetch = vi.fn(() =>
      Promise.resolve(
        new Response(
          JSON.stringify({
            error: { code: 'unavailable', message: 'stubbed' },
          }),
          { status: 503, headers: { 'content-type': 'application/json' } },
        ),
      ),
    ) as unknown as typeof globalThis.fetch
    const platform = {
      fetch,
      openSocket: vi.fn(socket),
      saveFile: vi.fn(() => Promise.resolve('saved' as const)),
      openExternal: vi.fn(),
      oauthClient: null,
      onDeepLink: null,
      onNativeFileDrop: null,
      defaultApiBaseUrl: null,
    }

    // A token, so the shell mounts signed-in and actually issues requests.
    storage.setItem('axon.token', 'tok-abc')
    render(<AppRoot platform={platform} storage={storage} />)

    await vi.waitFor(() => expect(platform.fetch).toHaveBeenCalled())

    // Every call must be the injected transport's, aimed at the stored server.
    const urls = (
      platform.fetch as unknown as { mock: { calls: unknown[][] } }
    ).mock.calls.map(([input]) =>
      typeof input === 'string' ? input : (input as Request).url,
    )
    expect(
      urls.some((url) => url.startsWith('https://axon.example.com/v1/')),
    ).toBe(true)
  })
})
