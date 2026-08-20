/**
 * `connectDeviceStateFlush` — device-state writes must survive the page going
 * away.
 *
 * Every write sits behind an 800 ms debounce, and nothing flushed it on unload:
 * `flushPending` was wired only into the auto-refresh path (ADR 0087), which
 * additionally does not install in dev. A reload inside that window dropped the
 * write, which presented as a thread the user had just opened coming back
 * unread on the next load.
 */
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  it,
  vi,
} from 'vitest'
import { createApiClient } from './api/client'
import { connectDeviceStateFlush } from './services'
import {
  createDeviceStateStore,
  THREAD_READ_MARKERS_NAMESPACE,
} from './stores/device-state'
import { createLiveConnection } from './stores/live-connection'
import { FakeWebSocket } from './test/fake-socket'
import { memoryStorage } from './test/memory-storage'

const ACCT = '11111111-1111-1111-1111-111111111111'
const ROOM = '!room:hs'
const ROOT = '$root'
const BASE = 'http://axon.test'

/** Namespaces seen by a `PUT`, in order. */
let puts: { namespace: string; entries: Record<string, unknown> }[] = []

const server = setupServer(
  http.get(`${BASE}/v1/devices/:deviceId/state/:namespace`, ({ params }) =>
    HttpResponse.json({
      data: { namespace: String(params.namespace), entries: {} },
    }),
  ),
  http.put(
    `${BASE}/v1/devices/:deviceId/state/:namespace`,
    async ({ request, params }) => {
      const body = (await request.json()) as {
        entries: Record<string, unknown>
      }
      puts.push({ namespace: String(params.namespace), entries: body.entries })
      return HttpResponse.json({ data: { updated_at: '2026-08-19T12:00:00Z' } })
    },
  ),
)
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  puts = []
  server.resetHandlers()
})
afterAll(() => server.close())

function harness() {
  const live = createLiveConnection({
    socketFactory: () => new FakeWebSocket().asWebSocket(),
  })
  const api = createApiClient(
    {
      getToken: () => 't',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE,
  )
  const deviceState = createDeviceStateStore(api, live, memoryStorage())
  const disconnect = connectDeviceStateFlush(deviceState)
  return { deviceState, disconnect }
}

/** Drive `visibilitychange` the way a reload does. */
function hidePage(): void {
  const original = Object.getOwnPropertyDescriptor(
    Document.prototype,
    'visibilityState',
  )
  Object.defineProperty(document, 'visibilityState', {
    configurable: true,
    get: () => 'hidden',
  })
  document.dispatchEvent(new Event('visibilitychange'))
  if (original !== undefined) {
    Object.defineProperty(Document.prototype, 'visibilityState', original)
  }
}

describe('connectDeviceStateFlush', () => {
  it('sends a debounced thread read marker when the page is hidden', async () => {
    const { deviceState, disconnect } = harness()
    deviceState.advanceThreadReadMarker(ACCT, ROOM, ROOT, '$reply', 1000)

    // Nothing has gone out yet: the write is behind the 800 ms debounce, which
    // is exactly the window a manual reload lands in.
    expect(puts).toEqual([])

    hidePage()

    // Well inside the 800 ms debounce: only the flush can satisfy this. A
    // default 1 s `waitFor` would be satisfied by the debounce firing on its
    // own, and passed with the listener removed.
    await vi.waitFor(
      () =>
        expect(
          puts.filter((p) => p.namespace === THREAD_READ_MARKERS_NAMESPACE),
        ).toHaveLength(1),
      { timeout: 200, interval: 10 },
    )
    disconnect()
  })

  it('sends on pagehide too', async () => {
    const { deviceState, disconnect } = harness()
    deviceState.setDraft(ACCT, ROOM, 'half-typed')
    expect(puts).toEqual([])

    window.dispatchEvent(new Event('pagehide'))

    await vi.waitFor(() => expect(puts).toHaveLength(1), {
      timeout: 200,
      interval: 10,
    })
    disconnect()
  })

  it('stops listening once disconnected', async () => {
    const { deviceState, disconnect } = harness()
    disconnect()
    deviceState.advanceThreadReadMarker(ACCT, ROOM, ROOT, '$reply', 1000)

    hidePage()
    window.dispatchEvent(new Event('pagehide'))

    await new Promise((resolve) => setTimeout(resolve, 50))
    expect(puts).toEqual([])
  })
})
