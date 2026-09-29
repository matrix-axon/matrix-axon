import { signal } from '@preact/signals'
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
import { PREFERENCES_CHANGED, type LiveFrame } from '../api/frames'
import type { LiveConnection } from './live-connection'
import { createSettingsStore } from './settings'
import {
  createSpaceOrderStore,
  parseSpaceOrder,
  validSpaceOrderEntry,
  type SpaceOrderStore,
} from './space-order'
import { memoryStorage } from '../test/memory-storage'

const BASE_URL = 'http://axon.test'
const URL = `${BASE_URL}/v1/preferences/space_order`
const DEVICE_ID = 'device-this'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function entry(roomId: string): string {
  return `${ACCOUNT}/${roomId}`
}

function preferenceBody(spaces: string[]) {
  return {
    data: {
      key: 'space_order',
      value: { spaces },
      updated_at: '2026-09-29T00:00:00Z',
    },
  }
}

function harness(initial: string[] = []) {
  const settings = createSettingsStore(memoryStorage())
  settings.spaceOrder.value = initial
  const reconnects = signal(0)
  const listeners = new Set<(frame: LiveFrame) => void>()
  const live = {
    reconnects,
    subscribe(listener: (frame: LiveFrame) => void) {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
  } as unknown as LiveConnection
  const api = createApiClient(
    {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
  const store: SpaceOrderStore = createSpaceOrderStore(
    api,
    live,
    DEVICE_ID,
    settings,
  )
  return {
    store,
    settings,
    reconnects,
    emit(value: unknown, deviceId = 'device-other') {
      listeners.forEach((listener) =>
        listener({
          type: PREFERENCES_CHANGED,
          accountId: '00000000-0000-0000-0000-000000000000',
          payload: {
            key: 'space_order',
            value,
            device_id: deviceId,
          },
        }),
      )
    },
  }
}

describe('validSpaceOrderEntry', () => {
  it('requires a UUID account and a room id', () => {
    expect(validSpaceOrderEntry(entry('!space:hs'))).toBe(true)
    expect(validSpaceOrderEntry(`not-a-uuid/!space:hs`)).toBe(false)
    expect(validSpaceOrderEntry(`${ACCOUNT}/room`)).toBe(false)
    expect(validSpaceOrderEntry(`${ACCOUNT}/!`)).toBe(false)
    expect(validSpaceOrderEntry(ACCOUNT)).toBe(false)
  })
})

describe('parseSpaceOrder', () => {
  it('rejects extra keys and malformed entries', () => {
    expect(parseSpaceOrder({ spaces: [entry('!space:hs')] })).toEqual([
      entry('!space:hs'),
    ])
    expect(parseSpaceOrder({ spaces: [entry('!space:hs')], extra: 1 })).toBe(
      null,
    )
    expect(parseSpaceOrder({ spaces: ['nope'] })).toBeNull()
    expect(parseSpaceOrder([])).toBeNull()
    expect(parseSpaceOrder(null)).toBeNull()
  })
})

describe('createSpaceOrderStore', () => {
  it('uploads a non-empty local list once when the preference is absent', async () => {
    const spaces = [entry('!a:hs'), 'not-a-space', entry('!b:hs')]
    const puts: unknown[] = []
    server.use(
      http.get(URL, () =>
        HttpResponse.json(
          { error: { code: 'not_found', message: 'preference not found' } },
          { status: 404 },
        ),
      ),
      http.put(URL, async ({ request }) => {
        puts.push(await request.json())
        return HttpResponse.json({ data: {} })
      }),
    )
    const { store, settings } = harness(spaces)

    await store.hydrate()

    await vi.waitFor(() => expect(puts).toHaveLength(1))
    expect(puts[0]).toEqual({
      device_id: DEVICE_ID,
      value: { spaces: [entry('!a:hs'), entry('!b:hs')] },
    })
    expect(settings.spaceOrder.value).toEqual(spaces)
  })

  it('does not upload an empty local list', async () => {
    let puts = 0
    server.use(
      http.get(URL, () =>
        HttpResponse.json(
          { error: { code: 'not_found', message: 'preference not found' } },
          { status: 404 },
        ),
      ),
      http.put(URL, () => {
        puts += 1
        return HttpResponse.json({ data: {} })
      }),
    )
    const { store } = harness([])

    await store.hydrate()
    await new Promise((resolve) => setTimeout(resolve, 20))

    expect(puts).toBe(0)
  })

  it('applies the server list from a 200', async () => {
    const serverSpaces = [entry('!server:hs')]
    let puts = 0
    server.use(
      http.get(URL, () => HttpResponse.json(preferenceBody(serverSpaces))),
      http.put(URL, () => {
        puts += 1
        return HttpResponse.json({ data: {} })
      }),
    )
    const { store, settings } = harness([entry('!local:hs')])

    await store.hydrate()

    expect(settings.spaceOrder.value).toEqual(serverSpaces)
    expect(puts).toBe(0)
  })

  it('keeps a move made before hydrate local, then uploads moves after it', async () => {
    const first = entry('!a:hs')
    const second = entry('!b:hs')
    const puts: unknown[] = []
    server.use(
      http.get(URL, () =>
        HttpResponse.json(
          { error: { code: 'not_found', message: 'preference not found' } },
          { status: 404 },
        ),
      ),
      http.put(URL, async ({ request }) => {
        puts.push(await request.json())
        return HttpResponse.json({ data: {} })
      }),
    )
    const { store, settings } = harness([first, second])

    store.move(second, 0, [first, second])
    expect(settings.spaceOrder.value).toEqual([second, first])
    expect(puts).toEqual([])

    await store.hydrate()
    await vi.waitFor(() => expect(puts).toHaveLength(1))
    expect(puts[0]).toEqual({
      device_id: DEVICE_ID,
      value: { spaces: [second, first] },
    })

    store.move(first, 0, [second, first])
    await vi.waitFor(() => expect(puts).toHaveLength(2))
    expect(puts[1]).toEqual({
      device_id: DEVICE_ID,
      value: { spaces: [first, second] },
    })
  })

  it('uploads a move that lands while hydrate is in flight', async () => {
    const first = entry('!a:hs')
    const second = entry('!b:hs')
    const serverSpaces = [entry('!server:hs')]
    let started = 0
    let release: () => void = () => {}
    const gate = new Promise<void>((resolve) => {
      release = resolve
    })
    const puts: unknown[] = []
    server.use(
      http.get(URL, async () => {
        started += 1
        await gate
        return HttpResponse.json(preferenceBody(serverSpaces))
      }),
      http.put(URL, async ({ request }) => {
        puts.push(await request.json())
        return HttpResponse.json({ data: {} })
      }),
    )
    const { store, settings } = harness([first, second])
    const pending = store.hydrate()
    await vi.waitFor(() => expect(started).toBe(1))

    store.move(second, 0, [first, second])
    release()
    await pending

    await vi.waitFor(() => expect(puts).toHaveLength(1))
    expect(settings.spaceOrder.value).toEqual([second, first])
    expect(puts[0]).toEqual({
      device_id: DEVICE_ID,
      value: { spaces: [second, first] },
    })
  })

  it('uploads a move when the first GET throws', async () => {
    const first = entry('!a:hs')
    const second = entry('!b:hs')
    let started = 0
    let release: () => void = () => {}
    const gate = new Promise<void>((resolve) => {
      release = resolve
    })
    const puts: unknown[] = []
    server.use(
      http.get(URL, async () => {
        started += 1
        await gate
        return HttpResponse.error()
      }),
      http.put(URL, async ({ request }) => {
        puts.push(await request.json())
        return HttpResponse.json({ data: {} })
      }),
    )
    const { store, settings } = harness([first, second])
    try {
      const pending = store.hydrate()
      await vi.waitFor(() => expect(started).toBe(1))
      store.move(second, 0, [first, second])
      release()
      await pending
      await vi.waitFor(() => expect(puts).toHaveLength(1))
      expect(settings.spaceOrder.value).toEqual([second, first])
      expect(puts[0]).toEqual({
        device_id: DEVICE_ID,
        value: { spaces: [second, first] },
      })
    } finally {
      store.resetSession()
    }
  })

  it('ignores this device and applies a sibling frame', () => {
    const local = [entry('!local:hs')]
    const sibling = [entry('!sibling:hs'), entry('!local:hs')]
    const { settings, emit } = harness(local)

    emit({ spaces: sibling }, DEVICE_ID)
    expect(settings.spaceOrder.value).toEqual(local)

    emit({ spaces: sibling })
    expect(settings.spaceOrder.value).toEqual(sibling)

    emit({ spaces: sibling, extra: true })
    expect(settings.spaceOrder.value).toEqual(sibling)
  })

  it('refetches when a sibling frame arrives during the upload', async () => {
    const local = [entry('!local:hs')]
    const sibling = [entry('!sibling:hs'), ...local]
    let gets = 0
    let release: () => void = () => {}
    const gate = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(URL, () => {
        gets += 1
        if (gets === 1) {
          return HttpResponse.json(
            { error: { code: 'not_found', message: 'preference not found' } },
            { status: 404 },
          )
        }
        return HttpResponse.json(preferenceBody(sibling))
      }),
      http.put(URL, async () => {
        await gate
        return HttpResponse.json({ data: {} })
      }),
    )
    const { store, settings, emit } = harness(local)
    const pending = store.hydrate()
    await vi.waitFor(() => expect(gets).toBe(1))

    emit({ spaces: sibling })
    expect(settings.spaceOrder.value).toEqual(local)

    release()
    await pending

    await vi.waitFor(() => expect(gets).toBe(2))
    expect(settings.spaceOrder.value).toEqual(sibling)
  })

  it('sends a move made during the refetch after a sibling frame', async () => {
    const first = entry('!a:hs')
    const second = entry('!b:hs')
    const sibling = [entry('!sibling:hs'), first, second]
    let gets = 0
    let releasePut: () => void = () => {}
    const putGate = new Promise<void>((resolve) => {
      releasePut = resolve
    })
    let releaseGet: () => void = () => {}
    const getGate = new Promise<void>((resolve) => {
      releaseGet = resolve
    })
    const puts: unknown[] = []
    server.use(
      http.get(URL, async () => {
        gets += 1
        if (gets === 1) {
          return HttpResponse.json(
            { error: { code: 'not_found', message: 'preference not found' } },
            { status: 404 },
          )
        }
        await getGate
        return HttpResponse.json(preferenceBody(sibling))
      }),
      http.put(URL, async ({ request }) => {
        puts.push(await request.json())
        if (puts.length === 1) {
          await putGate
        }
        return HttpResponse.json({ data: {} })
      }),
    )
    const { store, settings, emit } = harness([first, second])
    const pending = store.hydrate()
    await vi.waitFor(() => expect(puts).toHaveLength(1))

    emit({ spaces: sibling })
    releasePut()
    await vi.waitFor(() => expect(gets).toBe(2))

    store.move(second, 0, [first, second])
    expect(settings.spaceOrder.value).toEqual([second, first])
    releaseGet()
    await pending

    await vi.waitFor(() => expect(puts).toHaveLength(2))
    expect(settings.spaceOrder.value).toEqual([second, first])
    expect(puts[1]).toEqual({
      device_id: DEVICE_ID,
      value: { spaces: [second, first] },
    })
  })

  it('keeps a local order when a PUT fails and a reconnect returns the old list', async () => {
    const first = entry('!a:hs')
    const second = entry('!b:hs')
    const serverSpaces = [first, second]
    let gets = 0
    server.use(
      http.get(URL, () => {
        gets += 1
        return HttpResponse.json(preferenceBody(serverSpaces))
      }),
      http.put(URL, () =>
        HttpResponse.json(
          { error: { code: 'upstream', message: 'no' } },
          { status: 500 },
        ),
      ),
    )
    const { store, settings, reconnects } = harness([first, second])
    try {
      await store.hydrate()
      expect(settings.spaceOrder.value).toEqual(serverSpaces)

      store.move(second, 0, [first, second])
      expect(settings.spaceOrder.value).toEqual([second, first])
      await vi.waitFor(() =>
        expect(store.error.value).toBe('Could not save space order'),
      )

      reconnects.value = 1
      await vi.waitFor(() => expect(gets).toBe(2))
      expect(settings.spaceOrder.value).toEqual([second, first])
    } finally {
      store.resetSession()
    }
  })

  it('does not apply a reconnect GET that lands during a PUT', async () => {
    const first = entry('!a:hs')
    const second = entry('!b:hs')
    const serverSpaces = [first, second]
    let gets = 0
    let release: () => void = () => {}
    const gate = new Promise<void>((resolve) => {
      release = resolve
    })
    const puts: unknown[] = []
    server.use(
      http.get(URL, () => {
        gets += 1
        return HttpResponse.json(preferenceBody(serverSpaces))
      }),
      http.put(URL, async ({ request }) => {
        puts.push(await request.json())
        await gate
        return HttpResponse.json({ data: {} })
      }),
    )
    const { store, settings, reconnects } = harness([first, second])
    await store.hydrate()

    store.move(second, 0, [first, second])
    await vi.waitFor(() => expect(puts).toHaveLength(1))

    reconnects.value = 1
    await vi.waitFor(() => expect(gets).toBe(2))
    expect(settings.spaceOrder.value).toEqual([second, first])

    release()
    await vi.waitFor(() => expect(puts).toHaveLength(2))
    expect(settings.spaceOrder.value).toEqual([second, first])
    expect(puts[1]).toEqual({
      device_id: DEVICE_ID,
      value: { spaces: [second, first] },
    })
  })
})
