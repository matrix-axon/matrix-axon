import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from '@testing-library/preact'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { ServerSetup } from './ServerSetup'
import { SERVER_URL_KEY } from './server-url'
import { memoryStorage } from './test/memory-storage'

// testing-library's auto-cleanup needs vitest's injected globals, which this
// project does not enable — without this every render stacks up in one DOM.
afterEach(cleanup)

type FetchFn = typeof globalThis.fetch

const ok: FetchFn = () =>
  Promise.resolve(new Response('{"status":"ok"}', { status: 200 }))

const status =
  (code: number): FetchFn =>
  () =>
    Promise.resolve(new Response('nope', { status: code }))

function setup(fetchImpl: FetchFn) {
  const storage = memoryStorage()
  const onConnected = vi.fn()
  render(
    <ServerSetup
      onConnected={onConnected}
      platform={{ fetch: fetchImpl }}
      storage={storage}
    />,
  )
  const input = screen.getByLabelText('Server address')
  const connect = () =>
    screen.getByRole('button', { name: /^Connect/ }) as HTMLButtonElement
  return { storage, onConnected, input, connect }
}

describe('ServerSetup', () => {
  it('will not submit until the entry parses as a base URL', () => {
    const { input, connect } = setup(vi.fn(ok))
    expect(connect().disabled).toBe(true)

    fireEvent.input(input, { target: { value: 'javascript:alert(1)' } })
    expect(connect().disabled).toBe(true)

    fireEvent.input(input, { target: { value: 'axon.example.com' } })
    expect(connect().disabled).toBe(false)
  })

  it('shows what it will actually connect to, scheme filled in', () => {
    const { input } = setup(vi.fn(ok))
    fireEvent.input(input, { target: { value: 'axon.example.com' } })
    expect(
      screen.getByText(/will connect to https:\/\/axon\.example\.com/i),
    ).toBeTruthy()
  })

  it('submits a bare hostname — the field must not be type="url"', async () => {
    // Regression. `type="url"` applies native constraint validation, under
    // which `axon.example.com` is a typeMismatch, so the browser blocks the
    // submit *silently* — the button appears to do nothing. That defeats the
    // one affordance this screen advertises ("https:// is assumed"), and it
    // fails identically in a real browser and in jsdom.
    const fetchImpl = vi.fn(ok)
    const { input, connect, onConnected } = setup(fetchImpl)

    fireEvent.input(input, { target: { value: 'axon.example.com' } })
    expect((input as HTMLInputElement).checkValidity()).toBe(true)
    fireEvent.click(connect())

    await waitFor(() => expect(fetchImpl).toHaveBeenCalled())
    expect(onConnected).toHaveBeenCalledWith('https://axon.example.com')
  })

  it('probes /healthz and stores the server on success', async () => {
    const fetchImpl = vi.fn(ok)
    const { input, connect, storage, onConnected } = setup(fetchImpl)

    fireEvent.input(input, { target: { value: 'axon.example.com' } })
    fireEvent.click(connect())

    await waitFor(() =>
      expect(onConnected).toHaveBeenCalledWith('https://axon.example.com'),
    )
    // /healthz, not a /v1 route: it needs no token, and the user has none yet.
    expect(fetchImpl.mock.calls[0][0]).toBe('https://axon.example.com/healthz')
    expect(storage.getItem(SERVER_URL_KEY)).toBe('https://axon.example.com')
  })

  it('does not store a server it could not reach', async () => {
    const { input, connect, storage, onConnected } = setup(
      vi.fn((() =>
        Promise.reject(new TypeError('failed to fetch'))) as FetchFn),
    )
    fireEvent.input(input, { target: { value: 'axon.example.com' } })
    fireEvent.click(connect())

    await waitFor(() => expect(screen.getByRole('alert')).toBeTruthy())
    expect(screen.getByRole('alert').textContent).toMatch(/could not reach/i)
    // The whole point of probing: a stored-but-unreachable server drops the
    // user on a sign-in screen that cannot work, pointing at nothing.
    expect(storage.getItem(SERVER_URL_KEY)).toBeNull()
    expect(onConnected).not.toHaveBeenCalled()
  })

  it('rejects a host that answers but is not an Axon server', async () => {
    const { input, connect, storage, onConnected } = setup(vi.fn(status(404)))
    fireEvent.input(input, { target: { value: 'example.com' } })
    fireEvent.click(connect())

    await waitFor(() => expect(screen.getByRole('alert')).toBeTruthy())
    expect(screen.getByRole('alert').textContent).toMatch(/404/)
    expect(storage.getItem(SERVER_URL_KEY)).toBeNull()
    expect(onConnected).not.toHaveBeenCalled()
  })

  it('clears a previous failure as soon as the entry is edited', async () => {
    const { input, connect } = setup(vi.fn(status(500)))
    fireEvent.input(input, { target: { value: 'example.com' } })
    fireEvent.click(connect())
    await waitFor(() => expect(screen.getByRole('alert')).toBeTruthy())

    fireEvent.input(input, { target: { value: 'example.org' } })
    expect(screen.queryByRole('alert')).toBeNull()
  })

  it('gives up rather than hanging on a host that never answers', async () => {
    vi.useFakeTimers()
    try {
      const hangs: FetchFn = (_url, init) =>
        new Promise<Response>((_resolve, reject) => {
          init?.signal?.addEventListener('abort', () =>
            reject(new DOMException('aborted', 'AbortError')),
          )
        })
      const fetchImpl = vi.fn(hangs)
      const { input, connect, onConnected } = setup(fetchImpl)

      fireEvent.input(input, { target: { value: 'axon.example.com' } })
      fireEvent.click(connect())
      expect(connect().disabled).toBe(true)

      await vi.advanceTimersByTimeAsync(8000)
      await vi.waitFor(() => expect(screen.getByRole('alert')).toBeTruthy())
      expect(onConnected).not.toHaveBeenCalled()
    } finally {
      vi.useRealTimers()
    }
  })
})
