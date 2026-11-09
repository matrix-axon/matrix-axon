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
import { createApiClient } from '../api/client'
import { FakeWebSocket } from '../test/fake-socket'
import { memoryStorage } from '../test/memory-storage'
import {
  createDeviceStateStore,
  READ_MARKERS_NAMESPACE,
  type DeviceStateOptions,
} from './device-state'
import type {
  CachedDeviceStateScope,
  DeviceStateCache,
} from './device-state-cache'
import { createLiveConnection } from './live-connection'

/**
 * ADR 0131 on the web: reads after the first send `since`, a full read
 * replaces and a delta merges, the marker scopes persist with their cursor,
 * and a scope with no cursor waits for the room list before its full read.
 */

const BASE_URL = 'http://axon.test'
const ACCT = '6b53f7f0-0000-4000-8000-000000000001'
const STATE_PATH = `${BASE_URL}/v1/devices/:deviceId/state/:namespace`
const NS = READ_MARKERS_NAMESPACE

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  server.resetHandlers()
  vi.useRealTimers()
})
afterAll(() => server.close())

const marker = (eventId: string, originTs: number) => ({
  event_id: eventId,
  origin_ts: originTs,
})

const entry = (value: unknown) => ({
  value,
  device_id: 'sibling',
  updated_at: '2026-01-01T00:00:00Z',
})

const read = (
  full: boolean,
  cursor: string,
  entries: Record<string, unknown>,
) =>
  HttpResponse.json({
    data: {
      namespace: NS,
      full,
      cursor,
      entries: Object.fromEntries(
        Object.entries(entries).map(([key, value]) => [key, entry(value)]),
      ),
    },
  })

/** A scripted GET: records each `since`, answers from the queue in order. */
function scriptGets(...answers: Array<() => Response | Promise<Response>>) {
  const sinces: Array<string | null> = []
  server.use(
    http.get(STATE_PATH, ({ request }) => {
      sinces.push(new URL(request.url).searchParams.get('since'))
      const next = answers.shift()
      if (next === undefined) {
        throw new Error('unexpected GET')
      }
      return next()
    }),
  )
  return sinces
}

function memoryCache(seed?: CachedDeviceStateScope) {
  const writes: CachedDeviceStateScope[] = []
  const cache: DeviceStateCache = {
    read: (_accountId, namespace) =>
      Promise.resolve(namespace === NS ? seed : undefined),
    write: (_accountId, _namespace, scope) => {
      writes.push({ cursor: scope.cursor, entries: new Map(scope.entries) })
    },
  }
  return { cache, writes }
}

function setup(options: DeviceStateOptions = {}) {
  let socket: FakeWebSocket
  const live = createLiveConnection({
    socketFactory: () => {
      socket = new FakeWebSocket()
      return socket.asWebSocket()
    },
  })
  const api = createApiClient(
    {
      getToken: () => 'tok',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
  const store = createDeviceStateStore(api, live, memoryStorage(), options)
  let opened = false
  /** Drop and reopen the socket, which re-reads every hydrated scope. */
  async function reconnect() {
    if (!opened) {
      live.start()
      socket.emitOpen()
      opened = true
    }
    socket.emitClose()
    await vi.advanceTimersByTimeAsync(1000)
    socket.emitOpen()
    await vi.advanceTimersByTimeAsync(0)
  }
  return { store, reconnect }
}

describe('device-state since cursor (ADR 0131)', () => {
  it('sends the last cursor as since and merges the delta, deleting nulls', async () => {
    vi.useFakeTimers()
    const sinces = scriptGets(
      () =>
        read(true, '100', {
          '!a:hs': marker('$a1', 1),
          '!b:hs': marker('$b1', 1),
        }),
      () =>
        read(false, '200', {
          '!a:hs': null,
          '!c:hs': marker('$c1', 3),
        }),
    )
    const { store, reconnect } = setup()
    await store.ensureHydrated(ACCT, NS)
    expect(store.readMarker(ACCT, '!a:hs')?.eventId).toBe('$a1')

    await reconnect()
    expect(sinces).toEqual([null, '100'])
    expect(store.readMarker(ACCT, '!a:hs')).toBeNull()
    expect(store.readMarker(ACCT, '!b:hs')?.eventId).toBe('$b1')
    expect(store.readMarker(ACCT, '!c:hs')?.eventId).toBe('$c1')
  })

  it('replaces on a full read, but keeps a local write the server has not acked', async () => {
    vi.useFakeTimers()
    scriptGets(
      () =>
        read(true, '100', {
          '!a:hs': marker('$a1', 1),
          '!b:hs': marker('$b1', 1),
        }),
      // The cursor came from elsewhere, so the server answers in full.
      () => read(true, '300', { '!b:hs': marker('$b2', 2) }),
    )
    server.use(http.put(STATE_PATH, () => HttpResponse.error()))
    const { store, reconnect } = setup()
    await store.ensureHydrated(ACCT, NS)
    store.advanceReadMarker(ACCT, '!z:hs', '$z1', 9)

    await reconnect()
    expect(store.readMarker(ACCT, '!a:hs')).toBeNull()
    expect(store.readMarker(ACCT, '!b:hs')?.eventId).toBe('$b2')
    expect(store.readMarker(ACCT, '!z:hs')?.eventId).toBe('$z1')
  })

  it('sends no since to a server that returns no cursor', async () => {
    vi.useFakeTimers()
    const legacy = () =>
      HttpResponse.json({
        data: { namespace: NS, entries: { '!a:hs': entry(marker('$a1', 1)) } },
      })
    const sinces = scriptGets(legacy, legacy)
    const { store, reconnect } = setup()
    await store.ensureHydrated(ACCT, NS)
    await reconnect()
    expect(sinces).toEqual([null, null])
    expect(store.readMarker(ACCT, '!a:hs')?.eventId).toBe('$a1')
  })

  it('never overlaps two reads of one scope', async () => {
    vi.useFakeTimers()
    let releaseFirst!: () => void
    const firstHeld = new Promise<void>((resolve) => {
      releaseFirst = resolve
    })
    const sinces = scriptGets(
      async () => {
        await firstHeld
        return read(true, '100', { '!a:hs': marker('$a1', 1) })
      },
      () => read(false, '200', { '!a:hs': marker('$a2', 2) }),
    )
    const { store, reconnect } = setup()
    const first = store.ensureHydrated(ACCT, NS)
    await vi.advanceTimersByTimeAsync(0)
    // The reconnect re-read queues behind the slow full read, then reads the
    // delta from its cursor, rather than racing it and losing to it.
    await reconnect()
    expect(sinces).toEqual([null])
    releaseFirst()
    await first
    await vi.advanceTimersByTimeAsync(0)
    expect(sinces).toEqual([null, '100'])
    expect(store.readMarker(ACCT, '!a:hs')?.eventId).toBe('$a2')
  })
})

describe('device-state persistence (ADR 0131)', () => {
  it('restores a cached scope at once, then reads a delta from its cursor', async () => {
    let answer!: () => void
    const held = new Promise<void>((resolve) => {
      answer = resolve
    })
    const sinces = scriptGets(async () => {
      await held
      return read(false, '500', { '!b:hs': marker('$b2', 2) })
    })
    const { cache, writes } = memoryCache({
      cursor: '400',
      entries: new Map([
        ['!a:hs', marker('$a1', 1)],
        ['!b:hs', marker('$b1', 1)],
      ]),
    })
    const { store } = setup({ cache })
    const hydrating = store.ensureHydrated(ACCT, NS)

    await vi.waitFor(() =>
      expect(store.readMarker(ACCT, '!a:hs')?.eventId).toBe('$a1'),
    )
    // Shown, but the forward-only guards wait for an answered read.
    expect(store.hydrated(ACCT, NS)).toBe(false)

    answer()
    await hydrating
    expect(sinces).toEqual(['400'])
    expect(store.hydrated(ACCT, NS)).toBe(true)
    expect(store.readMarker(ACCT, '!b:hs')?.eventId).toBe('$b2')
    expect(writes).toHaveLength(1)
    expect(writes[0].cursor).toBe('500')
    expect([...writes[0].entries.keys()].sort()).toEqual(['!a:hs', '!b:hs'])
    expect(writes[0].entries.get('!b:hs')).toEqual(marker('$b2', 2))
  })

  it('persists the server view, not a local write the server has not acked', async () => {
    vi.useFakeTimers()
    scriptGets(
      () => read(true, '100', { '!a:hs': marker('$a1', 1) }),
      () => read(false, '200', { '!b:hs': marker('$b1', 1) }),
    )
    server.use(http.put(STATE_PATH, () => HttpResponse.error()))
    const { cache, writes } = memoryCache()
    const { store, reconnect } = setup({ cache })
    await store.ensureHydrated(ACCT, NS)
    store.advanceReadMarker(ACCT, '!a:hs', '$a9', 9)

    await reconnect()
    const last = writes.at(-1)
    expect(last?.cursor).toBe('200')
    expect(last?.entries.get('!a:hs')).toEqual(marker('$a1', 1))
    expect(store.readMarker(ACCT, '!a:hs')?.eventId).toBe('$a9')
  })

  it('writes nothing to disk for an empty delta', async () => {
    vi.useFakeTimers()
    scriptGets(
      () => read(true, '100', { '!a:hs': marker('$a1', 1) }),
      () => read(false, '200', {}),
    )
    const { cache, writes } = memoryCache()
    const { store, reconnect } = setup({ cache })
    await store.ensureHydrated(ACCT, NS)
    expect(writes).toHaveLength(1)
    await reconnect()
    expect(writes).toHaveLength(1)
  })

  it('a restored key loses to a local write made before the restore landed', async () => {
    scriptGets(() => read(false, '500', {}))
    server.use(
      http.put(STATE_PATH, () =>
        HttpResponse.json({ data: { updated_at: '2026-01-01T00:00:00Z' } }),
      ),
    )
    let release!: () => void
    const restoreHeld = new Promise<void>((resolve) => {
      release = resolve
    })
    const cache: DeviceStateCache = {
      read: async () => {
        await restoreHeld
        return {
          cursor: '400',
          entries: new Map([['!a:hs', marker('$a1', 1)]]),
        }
      },
      write: () => {},
    }
    const { store } = setup({ cache })
    const hydrating = store.ensureHydrated(ACCT, NS)
    store.advanceReadMarker(ACCT, '!a:hs', '$a7', 7)
    release()
    await hydrating
    expect(store.readMarker(ACCT, '!a:hs')?.eventId).toBe('$a7')
  })
})

describe('device-state full-read gate (ADR 0131)', () => {
  it('holds a first full read until the gate opens', async () => {
    const sinces = scriptGets(() =>
      read(true, '100', { '!a:hs': marker('$a1', 1) }),
    )
    let open!: () => void
    const gate = new Promise<void>((resolve) => {
      open = resolve
    })
    const gated: string[] = []
    const { store } = setup({
      beforeFullRead: (namespace) => {
        gated.push(namespace)
        return gate
      },
    })
    const hydrating = store.ensureHydrated(ACCT, NS)
    await new Promise((resolve) => setTimeout(resolve, 20))
    expect(gated).toEqual([NS])
    expect(sinces).toEqual([])
    expect(store.hydrateSettled(ACCT, NS)).toBe(false)

    open()
    await hydrating
    expect(sinces).toEqual([null])
    expect(store.hydrated(ACCT, NS)).toBe(true)
  })

  it('a failed gate still lets the read go', async () => {
    const sinces = scriptGets(() => read(true, '100', {}))
    const { store } = setup({
      beforeFullRead: () => Promise.reject(new Error('rooms failed')),
    })
    await store.ensureHydrated(ACCT, NS)
    expect(sinces).toEqual([null])
  })

  it('does not hold a scope restored with a cursor', async () => {
    const sinces = scriptGets(() => read(false, '500', {}))
    const { cache } = memoryCache({ cursor: '400', entries: new Map() })
    const { store } = setup({
      cache,
      beforeFullRead: () => new Promise<void>(() => {}),
    })
    await store.ensureHydrated(ACCT, NS)
    expect(sinces).toEqual(['400'])
  })
})
