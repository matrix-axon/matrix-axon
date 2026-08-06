import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import {
  afterAll,
  afterEach,
  beforeAll,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from 'vitest'
import { createApiClient } from '../api/client'
import { createMediaService } from '../media/media-service'
import { createTimelineStoreCache } from './timeline-cache'
import type { EventDto } from './timeline'

/**
 * The warm-store LRU (ADR 0085 phase 1). What these tests are really about is
 * the two ways a cache can be worse than no cache: handing back a slice that
 * can never be reconciled, and throwing away a send the user has not placed.
 */

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const OTHER_ACCOUNT = '6b53f7f0-0000-4000-8000-000000000002'
const ROOM = '!room:hs'

function event(id: string, ts: number, roomId: string = ROOM): EventDto {
  return {
    account_id: ACCOUNT,
    event_id: id,
    room_id: roomId,
    sender: '@alice:hs',
    origin_ts: ts,
    arrival_order: ts,
    type: 'm.room.message',
    body: `body of ${id}`,
    redacted: false,
    edited: false,
    edit_count: 0,
  } as EventDto
}

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

let revoked: string[]
beforeEach(() => {
  revoked = []
  vi.spyOn(URL, 'createObjectURL').mockImplementation(() => 'blob:preview')
  vi.spyOn(URL, 'revokeObjectURL').mockImplementation((url) => {
    revoked.push(url)
  })
})
afterEach(() => vi.restoreAllMocks())

function makeCache(limit?: number) {
  const auth = {
    getToken: () => 'tok-test',
    onAuthFailure: () => {},
    LoginBootstrap: () => null,
  }
  const api = createApiClient(auth, BASE_URL)
  const media = createMediaService({ auth, baseUrl: BASE_URL })
  return createTimelineStoreCache(api, media, limit)
}

const timelinePath = (roomId: string) =>
  `${BASE_URL}/v1/accounts/:accountId/rooms/${encodeURIComponent(roomId)}/timeline`
const SEND_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/send`
const UPLOAD_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/media/uploads`

/**
 * Store identity, as a boolean. `expect(store).toBe(other)` reads better but
 * chai's inspector throws while formatting the *failure* — a signal-backed
 * store has properties it cannot describe — so a real regression would report
 * `Cannot read properties of undefined (reading 'name')` and nothing else.
 */
function sameStore(actual: unknown, expected: unknown): boolean {
  return actual === expected
}

/** A store holding one failed echo — unsent work the cache must not destroy. */
async function withFailedEcho(store: {
  send(body: string): Promise<boolean>
}): Promise<void> {
  server.use(
    http.post(SEND_PATH, () =>
      HttpResponse.json(
        { error: { code: 'bad_gateway', message: 'homeserver down' } },
        { status: 502 },
      ),
    ),
  )
  expect(await store.send('unsent draft')).toBe(false)
}

describe('createTimelineStoreCache', () => {
  it('hands the same store back to a room re-entered in one session', async () => {
    let fetches = 0
    server.use(
      http.get(timelinePath(ROOM), () => {
        fetches += 1
        return HttpResponse.json({
          data: { events: [event('$1', 100)], next_cursor: 'c1' },
        })
      }),
    )
    const cache = makeCache()

    const first = cache.acquire(ACCOUNT, ROOM)
    await first.loadLatest()
    const second = cache.acquire(ACCOUNT, ROOM)

    expect(sameStore(second, first)).toBe(true)
    // The point of the phase: re-entry has events already, so it never goes
    // back through `loading` — and the head fetch that follows is a merge.
    expect(second.loading.value).toBe(false)
    expect(second.events.value.map((e) => e.event_id)).toEqual(['$1'])
    expect(fetches).toBe(1)
    expect(cache.size).toBe(1)
  })

  it('keys stores by account and room, so neither leaks into the other', () => {
    const cache = makeCache()

    const room = cache.acquire(ACCOUNT, ROOM)

    expect(sameStore(cache.acquire(ACCOUNT, '!other:hs'), room)).toBe(false)
    expect(sameStore(cache.acquire(OTHER_ACCOUNT, ROOM), room)).toBe(false)
    expect(cache.size).toBe(3)
  })

  it('evicts the least recently acquired room past the cap', () => {
    const cache = makeCache(2)

    const a = cache.acquire(ACCOUNT, '!a:hs')
    const b = cache.acquire(ACCOUNT, '!b:hs')
    // Re-acquiring `a` makes `b` the oldest, so the next room evicts `b`.
    expect(sameStore(cache.acquire(ACCOUNT, '!a:hs'), a)).toBe(true)
    cache.acquire(ACCOUNT, '!c:hs')

    expect(cache.size).toBe(2)
    // Order matters below: acquiring an evicted room *re-admits* it and
    // evicts the next oldest, so `a` is checked before the probe for `b`.
    expect(sameStore(cache.acquire(ACCOUNT, '!a:hs'), a)).toBe(true)
    expect(sameStore(cache.acquire(ACCOUNT, '!b:hs'), b)).toBe(false)
  })

  it('never evicts a store holding an unsent send, even over the cap', async () => {
    const cache = makeCache(1)

    const pinned = cache.acquire(ACCOUNT, ROOM)
    await withFailedEcho(pinned)
    cache.acquire(ACCOUNT, '!b:hs')
    cache.acquire(ACCOUNT, '!c:hs')

    // The cap yields rather than dropping a message the user still owns; only
    // the unpinned `!b:hs` was evicted.
    expect(cache.size).toBe(2)
    const again = cache.acquire(ACCOUNT, ROOM)
    expect(sameStore(again, pinned)).toBe(true)
    expect(again.events.value[0].localEcho?.status).toBe('failed')
    expect(revoked).toEqual([])
  })

  it('keeps a failed media echo, its file and its preview, across re-entry', async () => {
    // The expensive echo to lose: the retained `File` is what a retry
    // re-uploads, and the preview url is what the row renders. A text echo
    // could be retyped; these cannot be recovered from anywhere.
    server.use(
      http.post(UPLOAD_PATH, () =>
        HttpResponse.json(
          { error: { code: 'bad_gateway', message: 'offline' } },
          { status: 502 },
        ),
      ),
    )
    const cache = makeCache(1)
    const store = cache.acquire(ACCOUNT, ROOM)
    expect(
      await store.sendMedia(
        new File(['bytes'], 'cat.png', { type: 'image/png' }),
      ),
    ).toBe(false)

    // Two more rooms, so a cap of one would have evicted this store twice over.
    cache.acquire(ACCOUNT, '!b:hs')
    cache.acquire(ACCOUNT, '!c:hs')
    const again = cache.acquire(ACCOUNT, ROOM)

    const echo = again.events.value[0].localEcho
    expect(echo?.status).toBe('failed')
    expect(echo?.media?.file.name).toBe('cat.png')
    expect(echo?.media?.previewUrl).toBe('blob:preview')
    expect(revoked).toEqual([])
  })

  it('refuses to reuse a slice parked in history, which cannot be gap-filled', async () => {
    server.use(
      http.get(timelinePath(ROOM), ({ request }) => {
        const atTs = new URL(request.url).searchParams.get('at_ts')
        return HttpResponse.json({
          data:
            atTs === null
              ? { events: [event('$new', 900)], next_cursor: 'c1' }
              : { events: [event('$old', 100)], next_cursor: null },
        })
      }),
    )
    const cache = makeCache()

    const jumped = cache.acquire(ACCOUNT, ROOM)
    await jumped.loadLatest()
    await jumped.jumpTo(100)
    expect(jumped.atEnd.value).toBe(false)

    // `refreshHead` deliberately leaves a parked slice alone (WCR-05), so
    // reusing this one would re-open the room in history *and* never show
    // what arrived since. Re-entry starts over on the newest page.
    const reentry = cache.acquire(ACCOUNT, ROOM)
    expect(sameStore(reentry, jumped)).toBe(false)
    expect(reentry.events.value).toEqual([])
    expect(cache.size).toBe(1)
  })

  it('rebuilds at the head when a parked slice is pinned by unsent work', async () => {
    // The two rules meet here, and the wrong answer is invisible: a parked
    // slice must not be reused (it can never be gap-filled) but a store with an
    // unsent send must not be dropped. Reusing the parked slice would re-open
    // the room in old history with none of the messages since — while the echo
    // sat there looking fine.
    server.use(
      http.get(timelinePath(ROOM), ({ request }) => {
        const atTs = new URL(request.url).searchParams.get('at_ts')
        return HttpResponse.json({
          data:
            atTs === null
              ? { events: [event('$new', 900)], next_cursor: 'c1' }
              : { events: [event('$old', 100)], next_cursor: null },
        })
      }),
    )
    const cache = makeCache()
    const store = cache.acquire(ACCOUNT, ROOM)
    await store.loadLatest()
    await store.jumpTo(100)
    await withFailedEcho(store)
    expect(store.atEnd.value).toBe(false)
    expect(store.events.value.map((e) => e.event_id)).toEqual([
      '$old',
      expect.stringMatching(/^local:/) as unknown as string,
    ])

    const again = cache.acquire(ACCOUNT, ROOM)

    // Kept — the echo is unrebuildable — but no longer parked, so the head
    // load that follows re-entry is allowed to replace the history.
    expect(sameStore(again, store)).toBe(true)
    expect(again.atEnd.value).toBe(true)
    // Cursor-unknown, not exhausted: scroll-back must not be suppressed.
    expect(again.atStart.value).toBe(false)
    expect(again.events.value.map((e) => e.event_id)).toEqual([
      expect.stringMatching(/^local:/) as unknown as string,
    ])

    await again.loadLatest()

    // Current messages, with the unsent send still at the tail.
    expect(again.events.value.map((e) => e.event_id)).toEqual([
      '$new',
      expect.stringMatching(/^local:/) as unknown as string,
    ])
    expect(again.events.value[1].localEcho?.status).toBe('failed')
    expect(again.events.value[1].localEcho?.body).toBe('unsent draft')
    expect(revoked).toEqual([])
  })

  it('reuses a scrolled-back slice that still reaches the live tail', async () => {
    server.use(
      http.get(timelinePath(ROOM), ({ request }) => {
        const cursor = new URL(request.url).searchParams.get('cursor')
        return HttpResponse.json({
          data:
            cursor === null
              ? { events: [event('$new', 900)], next_cursor: 'c1' }
              : { events: [event('$old', 100)], next_cursor: null },
        })
      }),
    )
    const cache = makeCache()

    const store = cache.acquire(ACCOUNT, ROOM)
    await store.loadLatest()
    expect(await store.loadOlder()).toBe(true)

    expect(sameStore(cache.acquire(ACCOUNT, ROOM), store)).toBe(true)
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$old', '$new'])
  })

  it('clears a stale error banner on re-entry', async () => {
    server.use(
      http.get(timelinePath(ROOM), () =>
        HttpResponse.json({
          data: { events: [event('$1', 100)], next_cursor: null },
        }),
      ),
    )
    const cache = makeCache()

    const store = cache.acquire(ACCOUNT, ROOM)
    await store.loadLatest()
    store.error.value = 'Failed to send'

    // A banner from the last visit is not this visit's news; a failed echo, if
    // there is one, still renders its own row status.
    expect(cache.acquire(ACCOUNT, ROOM).error.value).toBe(null)
  })

  it('drops every store on clear, revoking media echo previews', async () => {
    server.use(
      http.post(UPLOAD_PATH, () =>
        HttpResponse.json(
          { error: { code: 'bad_request', message: 'nope' } },
          { status: 400 },
        ),
      ),
    )
    const cache = makeCache()
    const store = cache.acquire(ACCOUNT, ROOM)
    await store.sendMedia(new File(['bytes'], 'cat.png', { type: 'image/png' }))
    expect(store.events.value[0].localEcho?.media?.previewUrl).toBe(
      'blob:preview',
    )

    cache.clear()

    // Sign-out is the one caller: the unsent work is gone either way, and the
    // object url behind the preview would otherwise leak.
    expect(revoked).toEqual(['blob:preview'])
    expect(store.events.value).toEqual([])
    expect(cache.size).toBe(0)
    expect(sameStore(cache.acquire(ACCOUNT, ROOM), store)).toBe(false)
  })

  // Read by the auto-refresh policy (ADR 0087): an unsent echo lives only in
  // memory, so reloading to pick up a new build would destroy it.
  describe('hasUnsentWork', () => {
    it('is false for an empty cache and for stores with nothing unsent', () => {
      const cache = makeCache()
      expect(cache.hasUnsentWork).toBe(false)
      cache.acquire(ACCOUNT, ROOM)
      expect(cache.hasUnsentWork).toBe(false)
    })

    it('is true while any warm store holds an unsent send', async () => {
      const cache = makeCache()
      const store = cache.acquire(ACCOUNT, ROOM)
      await withFailedEcho(store)
      expect(cache.hasUnsentWork).toBe(true)
    })

    it('sees unsent work in a room other than the open one', async () => {
      const cache = makeCache()
      await withFailedEcho(cache.acquire(ACCOUNT, ROOM))
      cache.acquire(ACCOUNT, '!other:hs')
      expect(cache.hasUnsentWork).toBe(true)
    })

    it('goes false again once the cache is cleared', async () => {
      const cache = makeCache()
      await withFailedEcho(cache.acquire(ACCOUNT, ROOM))
      cache.clear()
      expect(cache.hasUnsentWork).toBe(false)
    })
  })
})
