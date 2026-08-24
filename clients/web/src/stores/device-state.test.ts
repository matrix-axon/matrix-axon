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
import { createDeviceStateStore } from './device-state'
import { createLiveConnection } from './live-connection'

const BASE_URL = 'http://axon.test'
const ACCT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const ROOT = '$root:hs'
const STATE_PATH = `${BASE_URL}/v1/devices/:deviceId/state/:namespace`

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function makeApi() {
  return createApiClient(
    {
      getToken: () => 'tok',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    BASE_URL,
  )
}

function setup(storage = memoryStorage()) {
  let socket: FakeWebSocket
  const live = createLiveConnection({
    socketFactory: () => {
      socket = new FakeWebSocket()
      return socket.asWebSocket()
    },
  })
  const store = createDeviceStateStore(makeApi(), live, storage)
  return { store, live, socket: () => socket, storage }
}

const draftsFrame = (deviceId: string, entries: Record<string, unknown>) =>
  JSON.stringify({
    type: 'device_state.changed',
    account_id: ACCT,
    payload: {
      device_id: deviceId,
      namespace: 'drafts',
      entries,
      updated_at: '2026-01-01T00:00:00Z',
    },
  })

describe('createDeviceStateStore', () => {
  it('mints a device id once and persists it across instances', () => {
    const storage = memoryStorage()
    const first = setup(storage).store.deviceId
    const second = setup(storage).store.deviceId
    expect(first).toBe(second)
    expect(storage.getItem('axon.device_id')).toBe(first)
  })

  it('hydrates a draft from the merged GET (account-scoped)', async () => {
    server.use(
      http.get(STATE_PATH, ({ params, request }) => {
        expect(params.namespace).toBe('drafts')
        expect(new URL(request.url).searchParams.get('account_id')).toBe(ACCT)
        return HttpResponse.json({
          data: {
            namespace: 'drafts',
            entries: {
              [ROOM]: {
                value: { text: 'hello' },
                device_id: 'sibling',
                updated_at: '2026-01-01T00:00:00Z',
              },
            },
          },
        })
      }),
    )
    const { store } = setup()
    expect(store.draft(ACCT, ROOM)).toBe('')
    store.hydrateDrafts(ACCT)
    await vi.waitFor(() => expect(store.draft(ACCT, ROOM)).toBe('hello'))
  })

  it('setDraft updates locally at once, then PUTs the merge after debounce', async () => {
    vi.useFakeTimers()
    let body: unknown
    server.use(
      http.put(STATE_PATH, async ({ request }) => {
        body = await request.json()
        return HttpResponse.json({
          data: { updated_at: '2026-01-01T00:00:00Z' },
        })
      }),
    )
    const { store } = setup()
    store.setDraft(ACCT, ROOM, 'draft text')
    expect(store.draft(ACCT, ROOM)).toBe('draft text')
    expect(body).toBeUndefined()

    await vi.advanceTimersByTimeAsync(800)
    expect(body).toEqual({ entries: { [ROOM]: { text: 'draft text' } } })
    vi.useRealTimers()
  })

  // Called before an automatic reload (ADR 0087). Drafts are durable, but only
  // once the PUT behind the 800 ms debounce has actually gone out — reloading
  // inside that window would drop the last thing the user typed.
  it('flushPending sends debounced writes at once and resolves after the PUT', async () => {
    const bodies: unknown[] = []
    server.use(
      http.put(STATE_PATH, async ({ request }) => {
        bodies.push(await request.json())
        return HttpResponse.json({
          data: { updated_at: '2026-01-01T00:00:00Z' },
        })
      }),
    )
    const { store } = setup()
    store.setDraft(ACCT, ROOM, 'half-typed')
    expect(bodies).toEqual([])

    await store.flushPending()

    // Resolved means sent *and* answered — no timer advance needed.
    expect(bodies).toEqual([{ entries: { [ROOM]: { text: 'half-typed' } } }])
  })

  it('flushPending covers every namespace with pending writes', async () => {
    const namespaces: string[] = []
    server.use(
      http.put(STATE_PATH, ({ params }) => {
        namespaces.push(String(params.namespace))
        return HttpResponse.json({
          data: { updated_at: '2026-01-01T00:00:00Z' },
        })
      }),
    )
    const { store } = setup()
    store.setDraft(ACCT, ROOM, 'text')
    store.advanceReadMarker(ACCT, ROOM, '$e', 1000)

    await store.flushPending()
    expect(namespaces.sort()).toEqual(['drafts', 'read_markers'])
  })

  it('flushPending on an idle store resolves without a request', async () => {
    let puts = 0
    server.use(
      http.put(STATE_PATH, () => {
        puts += 1
        return HttpResponse.json({
          data: { updated_at: '2026-01-01T00:00:00Z' },
        })
      }),
    )
    const { store } = setup()
    await expect(store.flushPending()).resolves.toBeUndefined()
    expect(puts).toBe(0)
  })

  /**
   * The server merge is last-write-wins by arrival, so two PUTs in flight at
   * once are a lost update the moment the network reorders them. Writes are
   * serialized per scope to make that impossible: the second request must not
   * even be issued until the first has landed.
   */
  it('never has two PUTs for one scope in flight at once', async () => {
    const started: string[] = []
    const finished: string[] = []
    let releaseFirst: (() => void) | undefined
    server.use(
      http.put(STATE_PATH, async ({ request }) => {
        const body = (await request.json()) as {
          entries: Record<string, { text: string }>
        }
        const text = body.entries[ROOM].text
        started.push(text)
        if (text === 'hel') {
          // Hold the first request open; the second must queue behind it.
          await new Promise<void>((resolve) => {
            releaseFirst = resolve
          })
        }
        finished.push(text)
        return HttpResponse.json({
          data: { updated_at: '2026-01-01T00:00:00Z' },
        })
      }),
    )
    const { store } = setup()

    store.setDraft(ACCT, ROOM, 'hel')
    const first = store.flushPending()
    await vi.waitFor(() => expect(started).toEqual(['hel']))

    store.setDraft(ACCT, ROOM, 'hello')
    const second = store.flushPending()
    // The second PUT has not been issued — it is waiting on the first.
    await Promise.resolve()
    expect(started).toEqual(['hel'])

    releaseFirst!()
    await Promise.all([first, second])

    // Issued in order, so the newer text is what the server saw last.
    expect(started).toEqual(['hel', 'hello'])
    expect(finished).toEqual(['hel', 'hello'])
  })

  /**
   * The reload hazard specifically: a PUT already in flight when auto-refresh
   * calls `flushPending` has to land before the tab goes away. Awaiting only
   * self-started batches let a stale write land after the reload.
   */
  it('flushPending waits for a write that was already in flight', async () => {
    let releaseFirst: (() => void) | undefined
    let settledFirst = false
    server.use(
      http.put(STATE_PATH, async ({ request }) => {
        const body = (await request.json()) as {
          entries: Record<string, { text: string }>
        }
        if (body.entries[ROOM].text === 'first') {
          await new Promise<void>((resolve) => {
            releaseFirst = resolve
          })
          settledFirst = true
        }
        return HttpResponse.json({
          data: { updated_at: '2026-01-01T00:00:00Z' },
        })
      }),
    )
    const { store } = setup()

    // Start a write and let it reach the server, then stop touching drafts —
    // so there is nothing *pending* when flushPending is called.
    store.setDraft(ACCT, ROOM, 'first')
    void store.flushPending()
    await vi.waitFor(() => expect(releaseFirst).toBeDefined())

    let flushed = false
    const flush = store.flushPending().then(() => {
      flushed = true
    })
    await Promise.resolve()
    expect(flushed).toBe(false) // still waiting on the in-flight write

    releaseFirst!()
    await flush
    expect(settledFirst).toBe(true)
  })

  // A flush that fails must behave exactly like a debounced one that fails:
  // the batch is re-queued, not lost, and the caller is not left hanging.
  it('flushPending resolves even when the PUT fails, and re-queues the write', async () => {
    vi.useFakeTimers()
    try {
      let attempts = 0
      server.use(
        http.put(STATE_PATH, () => {
          attempts += 1
          return attempts === 1
            ? HttpResponse.error()
            : HttpResponse.json({
                data: { updated_at: '2026-01-01T00:00:00Z' },
              })
        }),
      )
      const { store } = setup()
      store.setDraft(ACCT, ROOM, 'text')

      await store.flushPending()
      expect(attempts).toBe(1)
      expect(store.draft(ACCT, ROOM)).toBe('text')

      // The re-queue rescheduled a debounced flush, which then succeeds.
      await vi.advanceTimersByTimeAsync(800)
      expect(attempts).toBe(2)
    } finally {
      vi.useRealTimers()
    }
  })

  it('clearing a draft writes a null tombstone', async () => {
    vi.useFakeTimers()
    let body: unknown
    server.use(
      http.put(STATE_PATH, async ({ request }) => {
        body = await request.json()
        return HttpResponse.json({
          data: { updated_at: '2026-01-01T00:00:00Z' },
        })
      }),
    )
    const { store } = setup()
    store.setDraft(ACCT, ROOM, '')
    expect(store.draft(ACCT, ROOM)).toBe('')
    await vi.advanceTimersByTimeAsync(800)
    expect(body).toEqual({ entries: { [ROOM]: null } })
    vi.useRealTimers()
  })

  it('applies a sibling frame and suppresses our own echo', () => {
    const { store, live, socket } = setup()
    live.start()

    socket().emitMessage(
      draftsFrame(store.deviceId, { [ROOM]: { text: 'echo' } }),
    )
    expect(store.draft(ACCT, ROOM)).toBe('')

    socket().emitMessage(
      draftsFrame('sibling', { [ROOM]: { text: 'from sibling' } }),
    )
    expect(store.draft(ACCT, ROOM)).toBe('from sibling')

    socket().emitMessage(draftsFrame('sibling', { [ROOM]: null }))
    expect(store.draft(ACCT, ROOM)).toBe('')
  })

  it('re-reads hydrated scopes on reconnect (lossy bus)', async () => {
    vi.useFakeTimers()
    let gets = 0
    server.use(
      http.get(STATE_PATH, () => {
        gets += 1
        return HttpResponse.json({ data: { namespace: 'drafts', entries: {} } })
      }),
    )
    const { store, live, socket } = setup()
    store.hydrateDrafts(ACCT)
    await vi.advanceTimersByTimeAsync(0)
    expect(gets).toBe(1)

    live.start()
    socket().emitOpen()
    socket().emitClose() // → reconnecting, schedules retry
    await vi.advanceTimersByTimeAsync(1000)
    socket().emitOpen() // reopened → reconnects bumps → re-read
    await vi.advanceTimersByTimeAsync(0)
    expect(gets).toBe(2)
    vi.useRealTimers()
  })

  it('advances the read marker forward only', () => {
    // Fake timers keep the debounced PUT from firing (no msw handler needed).
    vi.useFakeTimers()
    const { store } = setup()
    store.advanceReadMarker(ACCT, ROOM, '$e1', 100)
    expect(store.readMarker(ACCT, ROOM)).toEqual({
      eventId: '$e1',
      originTs: 100,
    })

    store.advanceReadMarker(ACCT, ROOM, '$older', 50)
    store.advanceReadMarker(ACCT, ROOM, '$same', 100)
    expect(store.readMarker(ACCT, ROOM)).toEqual({
      eventId: '$e1',
      originTs: 100,
    })

    store.advanceReadMarker(ACCT, ROOM, '$e2', 200)
    expect(store.readMarker(ACCT, ROOM)).toEqual({
      eventId: '$e2',
      originTs: 200,
    })
    vi.useRealTimers()
  })

  it('hydrates and advances thread read markers forward only', async () => {
    vi.useFakeTimers()
    let body: unknown
    server.use(
      http.get(STATE_PATH, ({ params }) => {
        expect(params.namespace).toBe('thread_read_markers')
        return HttpResponse.json({
          data: {
            namespace: 'thread_read_markers',
            entries: {
              [`${encodeURIComponent(ROOM)}:${encodeURIComponent(ROOT)}`]: {
                value: {
                  room_id: ROOM,
                  root_event_id: ROOT,
                  event_id: '$reply1',
                  origin_ts: 100,
                },
                device_id: 'sibling',
                updated_at: '2026-01-01T00:00:00Z',
              },
            },
          },
        })
      }),
      http.put(STATE_PATH, async ({ request }) => {
        body = await request.json()
        return HttpResponse.json({
          data: { updated_at: '2026-01-01T00:00:00Z' },
        })
      }),
    )
    const { store } = setup()
    store.hydrateThreadReadMarkers(ACCT)
    await vi.advanceTimersByTimeAsync(0)
    expect(store.threadReadMarker(ACCT, ROOM, ROOT)).toEqual({
      roomId: ROOM,
      rootEventId: ROOT,
      eventId: '$reply1',
      originTs: 100,
      // A marker written before the field existed parses as `null`, which the
      // receipt path treats as "no arrival evidence" rather than zero.
      arrivalThrough: null,
    })

    store.advanceThreadReadMarker(ACCT, ROOM, ROOT, '$older', 50)
    expect(store.threadReadMarker(ACCT, ROOM, ROOT)?.eventId).toBe('$reply1')
    store.advanceThreadReadMarker(ACCT, ROOM, ROOT, '$reply2', 200, 7)
    expect(store.threadReadMarker(ACCT, ROOM, ROOT)).toEqual({
      roomId: ROOM,
      rootEventId: ROOT,
      eventId: '$reply2',
      originTs: 200,
      arrivalThrough: 7,
    })

    // The two positions advance independently: a backfilled reply raises how far
    // the panel has read in arrival order while leaving the display position
    // where it is.
    store.advanceThreadReadMarker(ACCT, ROOM, ROOT, '$backfilled', 150, 9)
    expect(store.threadReadMarker(ACCT, ROOM, ROOT)).toEqual({
      roomId: ROOM,
      rootEventId: ROOT,
      eventId: '$reply2',
      originTs: 200,
      arrivalThrough: 9,
    })

    await vi.advanceTimersByTimeAsync(800)
    expect(body).toEqual({
      entries: {
        [`${encodeURIComponent(ROOM)}:${encodeURIComponent(ROOT)}`]: {
          room_id: ROOM,
          root_event_id: ROOT,
          event_id: '$reply2',
          origin_ts: 200,
          arrival_through: 9,
        },
      },
    })
    vi.useRealTimers()
  })

  it('scopes the revision counter to one namespace', () => {
    const { store } = setup()
    const before = store.revision(ACCT, 'thread_read_markers')

    // A draft keystroke must not invalidate a memo that only reads thread
    // markers — a single global counter made every character re-run the
    // receipt scan over the room's timeline (review).
    store.setDraft(ACCT, ROOM, 'h')
    store.setDraft(ACCT, ROOM, 'he')
    expect(store.revision(ACCT, 'thread_read_markers')).toBe(before)
    expect(store.revision(ACCT, 'drafts')).toBeGreaterThan(0)

    store.advanceThreadReadMarker(ACCT, ROOM, ROOT, '$reply', 100, 5)
    expect(store.revision(ACCT, 'thread_read_markers')).toBeGreaterThan(before)
  })

  it('reports a failed hydration as settled but not hydrated', async () => {
    vi.useFakeTimers()
    server.use(
      http.get(STATE_PATH, () => new HttpResponse(null, { status: 500 })),
    )
    const { store } = setup()
    store.hydrateThreadReadMarkers(ACCT)
    await vi.advanceTimersByTimeAsync(0)

    // The distinction a room badge needs: the fetch is over, so stop waiting,
    // but no data arrived, so a receipt must not claim on it.
    expect(store.hydrateSettled(ACCT, 'thread_read_markers')).toBe(true)
    expect(store.hydrated(ACCT, 'thread_read_markers')).toBe(false)
    vi.useRealTimers()
  })

  it('re-queues a network-failed PUT and retries after the next debounce (WCR-12)', async () => {
    vi.useFakeTimers()
    const bodies: unknown[] = []
    let failNext = true
    server.use(
      http.put(STATE_PATH, async ({ request }) => {
        bodies.push(await request.json())
        if (failNext) {
          failNext = false
          return HttpResponse.error()
        }
        return HttpResponse.json({
          data: { updated_at: '2026-01-01T00:00:00Z' },
        })
      }),
    )
    const { store } = setup()
    store.setDraft(ACCT, ROOM, 'unsent draft')

    // First flush: the PUT never reaches the server; the batch re-queues.
    await vi.advanceTimersByTimeAsync(800)
    expect(bodies).toHaveLength(1)
    // The local cache still shows the write while the retry is pending.
    expect(store.draft(ACCT, ROOM)).toBe('unsent draft')

    // Second debounce period: the same batch is retried and lands.
    await vi.advanceTimersByTimeAsync(800)
    expect(bodies).toHaveLength(2)
    expect(bodies[1]).toEqual({ entries: { [ROOM]: { text: 'unsent draft' } } })

    // Settled: no further PUTs fire.
    await vi.advanceTimersByTimeAsync(2000)
    expect(bodies).toHaveLength(2)
    vi.useRealTimers()
  })

  it('a write during the failed PUT wins over the re-queued value', async () => {
    vi.useFakeTimers()
    const bodies: unknown[] = []
    let fail = true
    server.use(
      http.put(STATE_PATH, async ({ request }) => {
        bodies.push(await request.json())
        if (fail) {
          fail = false
          return HttpResponse.error()
        }
        return HttpResponse.json({
          data: { updated_at: '2026-01-01T00:00:00Z' },
        })
      }),
    )
    const { store } = setup()
    store.setDraft(ACCT, ROOM, 'first')
    await vi.advanceTimersByTimeAsync(800) // fails, re-queues 'first'
    store.setDraft(ACCT, ROOM, 'second') // newer write for the same key
    await vi.advanceTimersByTimeAsync(800)
    expect(bodies.at(-1)).toEqual({ entries: { [ROOM]: { text: 'second' } } })
    vi.useRealTimers()
  })

  it('a reconnect re-read does not clobber an unsynced draft edit', async () => {
    vi.useFakeTimers()
    // The server holds what it acked before the outage; the client typed more
    // while offline, so the re-read must not roll the draft back to 'hello
    // there'. The PUT stays down across the re-read: connectivity returns, the
    // GET lands, and only then does the re-queued write get through.
    let stored = 'hello'
    let online = true
    server.use(
      http.get(STATE_PATH, () =>
        HttpResponse.json({
          data: {
            namespace: 'drafts',
            entries: {
              [ROOM]: {
                value: { text: stored },
                device_id: 'self',
                updated_at: '2026-01-01T00:00:00Z',
              },
            },
          },
        }),
      ),
      http.put(STATE_PATH, async ({ request }) => {
        const body = (await request.json()) as {
          entries: Record<string, { text: string } | null>
        }
        if (!online) {
          return HttpResponse.error()
        }
        stored = body.entries[ROOM]?.text ?? ''
        return HttpResponse.json({
          data: { updated_at: '2026-01-01T00:00:00Z' },
        })
      }),
    )
    const { store, live, socket } = setup()
    store.hydrateDrafts(ACCT)
    await vi.advanceTimersByTimeAsync(0)
    expect(store.draft(ACCT, ROOM)).toBe('hello')

    // Typed and synced, then the network drops mid-composition.
    store.setDraft(ACCT, ROOM, 'hello there')
    await vi.advanceTimersByTimeAsync(800)
    expect(stored).toBe('hello there')
    live.start()
    socket().emitOpen()
    online = false
    store.setDraft(ACCT, ROOM, 'hello there, offline tail')
    await vi.advanceTimersByTimeAsync(800) // PUT fails, batch re-queues

    // The socket comes back and the reconnect re-read lands first.
    socket().emitClose()
    await vi.advanceTimersByTimeAsync(1000) // retry PUT still failing
    socket().emitOpen() // reconnects bumps → re-GET of the drafts scope
    await vi.advanceTimersByTimeAsync(0)
    expect(store.draft(ACCT, ROOM)).toBe('hello there, offline tail')

    // Then the re-queued write finally gets through, carrying the full draft.
    online = true
    await vi.advanceTimersByTimeAsync(800)
    expect(stored).toBe('hello there, offline tail')
    expect(store.draft(ACCT, ROOM)).toBe('hello there, offline tail')
    vi.useRealTimers()
  })

  it('a re-read issued before its ack does not roll a draft back', async () => {
    vi.useFakeTimers()
    // The subtler race: the GET is issued while the write is unacked, the PUT
    // is acked while the GET is in flight, and the response — computed before
    // the PUT landed — still carries the old text. It must not be applied.
    let release = () => {}
    const inFlight = new Promise<void>((resolve) => {
      release = resolve
    })
    server.use(
      http.get(STATE_PATH, async () => {
        await inFlight
        return HttpResponse.json({
          data: {
            namespace: 'drafts',
            entries: {
              [ROOM]: {
                value: { text: 'stale' },
                device_id: 'self',
                updated_at: '2026-01-01T00:00:00Z',
              },
            },
          },
        })
      }),
      http.put(STATE_PATH, () =>
        HttpResponse.json({ data: { updated_at: '2026-01-01T00:00:00Z' } }),
      ),
    )
    const { store } = setup()
    store.setDraft(ACCT, ROOM, 'fresh local text')
    store.hydrateDrafts(ACCT) // GET issued with the write still unacked
    await vi.advanceTimersByTimeAsync(800) // PUT acks 'fresh local text'
    release()
    await vi.advanceTimersByTimeAsync(0)

    expect(store.draft(ACCT, ROOM)).toBe('fresh local text')
    vi.useRealTimers()
  })

  it("a sibling's frame does not clobber an unsynced local draft edit", async () => {
    vi.useFakeTimers()
    server.use(http.put(STATE_PATH, () => HttpResponse.error()))
    const { store, live, socket } = setup()
    live.start()
    socket().emitOpen()

    store.setDraft(ACCT, ROOM, 'typing offline')
    await vi.advanceTimersByTimeAsync(800) // PUT fails; the write is unacked
    socket().emitMessage(draftsFrame('sibling', { [ROOM]: { text: 'theirs' } }))
    expect(store.draft(ACCT, ROOM)).toBe('typing offline')
    vi.useRealTimers()
  })

  it('re-queues a network-failed batch read-marker PUT', async () => {
    vi.useFakeTimers()
    const bodies: unknown[] = []
    let failNext = true
    server.use(
      http.put(STATE_PATH, async ({ request }) => {
        bodies.push(await request.json())
        if (failNext) {
          failNext = false
          return HttpResponse.error()
        }
        return HttpResponse.json({
          data: { updated_at: '2026-01-01T00:00:00Z' },
        })
      }),
    )
    const { store } = setup()

    await store.markRoomSummariesRead(ACCT, [
      {
        account_id: ACCT,
        room_id: ROOM,
        last_event_id: '$latest',
        last_activity_ts: 200,
      } as never,
    ])

    expect(bodies).toHaveLength(1)
    expect(store.readMarker(ACCT, ROOM)).toEqual({
      eventId: '$latest',
      originTs: 200,
    })

    await vi.advanceTimersByTimeAsync(800)
    expect(bodies).toHaveLength(2)
    expect(bodies[1]).toEqual({
      entries: { [ROOM]: { event_id: '$latest', origin_ts: 200 } },
    })
    vi.useRealTimers()
  })
})
