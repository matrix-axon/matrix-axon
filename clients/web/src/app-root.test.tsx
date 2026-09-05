import { cleanup, render, screen } from '@testing-library/preact'
import { afterEach, describe, expect, it } from 'vitest'
import { AppRoot } from './app-root'
import { browserPlatform, type Platform } from './platform'
import { resolveApiBaseUrl } from './services'
import { SERVER_URL_KEY } from './server-url'
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
