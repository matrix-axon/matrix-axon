import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { createApiClient } from '../api/client'
import type { SearchQuery } from '../search-tokens'
import { createSearchStore, type SearchResult } from './search'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const SEARCH_PATH = `${BASE_URL}/v1/search`

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function makeStore() {
  const auth = {
    getToken: () => 'tok-test',
    onAuthFailure: () => {},
    LoginBootstrap: () => null,
  }
  return createSearchStore(createApiClient(auth, BASE_URL))
}

function query(text: string): SearchQuery {
  return {
    text,
    scope: { kind: 'room', accountId: ACCOUNT, roomId: '!room:hs' },
    sender: null,
    from: null,
    to: null,
  }
}

function hit(id: string, score = 1): SearchResult {
  return {
    score,
    event: {
      account_id: ACCOUNT,
      event_id: id,
      room_id: '!room:hs',
      sender: '@alice:hs',
      origin_ts: 1000,
      arrival_order: 1000,
      type: 'm.room.message',
      body: `body of ${id}`,
      redacted: false,
      edited: false,
      edit_count: 0,
    } as SearchResult['event'],
  }
}

describe('createSearchStore', () => {
  it('runs a query and exposes the first page', async () => {
    let seen: URLSearchParams | null = null
    server.use(
      http.get(SEARCH_PATH, ({ request }) => {
        seen = new URL(request.url).searchParams
        return HttpResponse.json({
          data: {
            results: [hit('$1'), hit('$2')],
            total: 7,
            next_cursor: 'c1',
          },
        })
      }),
    )
    const store = makeStore()
    await store.run(query('deploy'))
    expect(seen!.get('q')).toBe('deploy')
    expect(seen!.get('account_id')).toBe(ACCOUNT)
    expect(seen!.get('room_id')).toBe('!room:hs')
    expect(seen!.get('limit')).toBe('50')
    expect(store.results.value.map((r) => r.event.event_id)).toEqual([
      '$1',
      '$2',
    ])
    expect(store.total.value).toBe(7)
    expect(store.exhausted.value).toBe(false)
    expect(store.lastQuery.value).toEqual(query('deploy'))
    store.clear()
    expect(store.lastQuery.value).toBeNull()
    store.preserveForResultJump()
    expect(store.consumeResultJumpPreservation()).toBe(true)
    expect(store.consumeResultJumpPreservation()).toBe(false)
    expect(store.loading.value).toBe(false)
  })

  it('appends the next page via the cursor and stops at the end', async () => {
    server.use(
      http.get(SEARCH_PATH, ({ request }) => {
        const cursor = new URL(request.url).searchParams.get('cursor')
        return HttpResponse.json({
          data:
            cursor === null
              ? { results: [hit('$1')], total: 2, next_cursor: 'c1' }
              : { results: [hit('$2')], total: 2, next_cursor: null },
        })
      }),
    )
    const store = makeStore()
    await store.run(query('x'))
    await store.loadMore()
    expect(store.results.value.map((r) => r.event.event_id)).toEqual([
      '$1',
      '$2',
    ])
    expect(store.exhausted.value).toBe(true)
    // A further loadMore is a no-op, not a request (onUnhandledRequest would
    // fail the test if one escaped).
    await store.loadMore()
  })

  it('discards a loadMore that a newer run supersedes', async () => {
    let releaseFirst: (() => void) | null = null
    const gate = new Promise<void>((resolve) => {
      releaseFirst = resolve
    })
    server.use(
      http.get(SEARCH_PATH, async ({ request }) => {
        const params = new URL(request.url).searchParams
        if (params.get('cursor') === 'c1') {
          await gate
          return HttpResponse.json({
            data: { results: [hit('$stale')], total: 9, next_cursor: 'c2' },
          })
        }
        return HttpResponse.json({
          data: {
            results: [hit(params.get('q') === 'first' ? '$1' : '$fresh')],
            total: 1,
            next_cursor: params.get('q') === 'first' ? 'c1' : null,
          },
        })
      }),
    )
    const store = makeStore()
    await store.run(query('first'))
    const stale = store.loadMore()
    await store.run(query('second'))
    releaseFirst!()
    await stale
    expect(store.results.value.map((r) => r.event.event_id)).toEqual(['$fresh'])
    expect(store.loadingMore.value).toBe(false)
  })

  it('reports the error envelope message on a 400', async () => {
    server.use(
      http.get(SEARCH_PATH, () =>
        HttpResponse.json(
          { error: { code: 'bad_request', message: 'unbounded empty query' } },
          { status: 400 },
        ),
      ),
    )
    const store = makeStore()
    await store.run(query(''))
    expect(store.error.value).toBe('unbounded empty query')
    expect(store.unavailable.value).toBe(false)
    expect(store.results.value).toEqual([])
  })

  it('maps a 503 to unavailable, not error', async () => {
    server.use(
      http.get(SEARCH_PATH, () =>
        HttpResponse.json(
          { error: { code: 'unavailable', message: 'search is disabled' } },
          { status: 503 },
        ),
      ),
    )
    const store = makeStore()
    await store.run(query('x'))
    expect(store.unavailable.value).toBe(true)
    expect(store.error.value).toBeNull()
  })

  it('surfaces a network-level failure as an error', async () => {
    server.use(http.get(SEARCH_PATH, () => HttpResponse.error()))
    const store = makeStore()
    await store.run(query('x'))
    expect(store.error.value).not.toBeNull()
    expect(store.loading.value).toBe(false)
  })

  it('clear returns to the never-searched state', async () => {
    server.use(
      http.get(SEARCH_PATH, () =>
        HttpResponse.json({
          data: { results: [hit('$1')], total: 1, next_cursor: null },
        }),
      ),
    )
    const store = makeStore()
    await store.run(query('x'))
    store.clear()
    expect(store.results.value).toEqual([])
    expect(store.total.value).toBeNull()
    expect(store.error.value).toBeNull()
  })
})
