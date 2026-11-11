/**
 * The room-details store's space-hierarchy pager, crawl enrichment and
 * one-room preview (ADR 0133 decisions 6–8). The detail reads it shares a
 * store with are covered in `room-details.test.ts`.
 */
import { signal } from '@preact/signals'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, expect, it, vi } from 'vitest'
import { createApiClient } from '../api/client'
import {
  ROOM_DETAILS_CHANGED,
  TIMELINE_EVENT,
  type LiveFrame,
} from '../api/frames'
import {
  discoveredRoom,
  hierarchyChild,
  hierarchyPage,
  homeserverDetailsDto,
  metadataDto,
  roomDiscovery,
  spaceDiscovery,
  type RoomDiscoveryDto,
  type SpaceDiscoveryDto,
  type SpaceHierarchyPage,
} from '../test/room-details'
import type { LiveConnection } from './live-connection'
import {
  CRAWL_RETRY_MAX_MS,
  CRAWL_RETRY_MIN_MS,
  createRoomDetailsStore,
  mergeRereads,
  rereadFor,
  rereadPrompted,
} from './room-details'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const OTHER = '6b53f7f0-0000-4000-8000-000000000002'
const SPACE = '!space:hs'
const NEXT = '!next:hs'
const CHILD = '!child:hs'

const server = setupServer()
const disposers: Array<() => void> = []
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  for (const dispose of disposers.splice(0)) dispose()
  server.resetHandlers()
  vi.useRealTimers()
})
afterAll(() => server.close())

interface HierarchyRequest {
  roomId: string
  limit: number
  cursor: string | null
  enrich: boolean
}

/** `count` children named `prefix-0`, `prefix-1`, …, in order. */
function rows(count: number, prefix = 'r', from = 0) {
  return Array.from({ length: count }, (_, i) =>
    hierarchyChild(`!${prefix}-${from + i}:hs`),
  )
}

/**
 * A store against msw, with the hierarchy and discovery answers supplied by
 * the test and every request recorded. `hold()` keeps hierarchy responses
 * open until `release()`.
 */
function harness(
  options: {
    hierarchy?: (request: HierarchyRequest) => SpaceHierarchyPage
    discovery?: (roomId: string) => RoomDiscoveryDto
  } = {},
) {
  const hierarchyRequests: HierarchyRequest[] = []
  const discoveryRequests: string[] = []
  const held: Array<() => void> = []
  let holding = false
  const base = `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId`
  server.use(
    http.get(`${base}/metadata`, () =>
      HttpResponse.json({ data: metadataDto() }),
    ),
    http.get(`${base}/info`, () =>
      HttpResponse.json({ data: { member_counts: null } }),
    ),
    http.get(`${base}/homeserver_details`, () =>
      HttpResponse.json({ data: homeserverDetailsDto() }),
    ),
    http.get(`${base}/space/hierarchy`, ({ params, request }) => {
      const url = new URL(request.url)
      const parsed: HierarchyRequest = {
        roomId: decodeURIComponent(String(params.roomId)),
        limit: Number(url.searchParams.get('limit')),
        cursor: url.searchParams.get('cursor'),
        enrich: url.searchParams.get('enrich') === 'true',
      }
      hierarchyRequests.push(parsed)
      const body = () =>
        HttpResponse.json({
          data: (options.hierarchy ?? (() => hierarchyPage([])))(parsed),
        })
      if (!holding) return body()
      return new Promise<Response>((resolve) =>
        held.push(() => resolve(body())),
      )
    }),
    http.get(`${base}/discovery`, ({ params }) => {
      const roomId = decodeURIComponent(String(params.roomId))
      discoveryRequests.push(roomId)
      return HttpResponse.json({
        data: (options.discovery ?? ((id) => roomDiscovery(id)))(roomId),
      })
    }),
  )
  const listeners = new Set<(frame: LiveFrame) => void>()
  const reconnects = signal(0)
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
  const store = createRoomDetailsStore(api, live)
  disposers.push(() => store.dispose())
  const emit = (frame: LiveFrame) => {
    for (const listener of listeners) listener(frame)
  }
  return {
    store,
    hierarchyRequests,
    discoveryRequests,
    frame: (sources: readonly string[], roomId = SPACE, accountId = ACCOUNT) =>
      emit({
        type: ROOM_DETAILS_CHANGED,
        accountId,
        payload: { room_id: roomId, sources },
      }),
    timeline: (type: string, roomId = SPACE) =>
      emit({
        type: TIMELINE_EVENT,
        accountId: ACCOUNT,
        payload: {
          account_id: ACCOUNT,
          room_id: roomId,
          type,
          event_id: '$e',
          sender: '@a:hs',
          origin_server_ts: 0,
          content: {},
        },
      }),
    reconnect: () => {
      reconnects.value += 1
    },
    hold: () => {
      holding = true
    },
    release: () => {
      holding = false
      for (const resolve of held.splice(0)) resolve()
    },
    heldCount: () => held.length,
    list: () => store.state.value.hierarchy.value,
  }
}

type Harness = ReturnType<typeof harness>

const showSpace = (h: Harness, roomId = SPACE, joined = true) =>
  h.store.show({ accountId: ACCOUNT, roomId, joined, space: true })

/** Let msw answer everything already asked, and the store apply it. */
async function settle(h: Harness, reads: number): Promise<void> {
  await vi.waitFor(() => expect(h.hierarchyRequests).toHaveLength(reads))
  await vi.waitFor(() => expect(h.heldCount()).toBe(0))
  await new Promise((resolve) => setTimeout(resolve, 0))
  await new Promise((resolve) => setTimeout(resolve, 0))
}

it('reads the first page of a joined space with enrich, 50 rows', async () => {
  const h = harness({ hierarchy: () => hierarchyPage(rows(3)) })
  showSpace(h)
  await vi.waitFor(() => expect(h.list()?.children).toHaveLength(3))
  expect(h.hierarchyRequests).toEqual([
    { roomId: SPACE, limit: 50, cursor: null, enrich: true },
  ])
})

it('never enriches a space the account has not joined', async () => {
  const h = harness({ hierarchy: () => hierarchyPage(rows(1)) })
  showSpace(h, SPACE, false)
  await vi.waitFor(() => expect(h.list()).not.toBeNull())
  expect(h.hierarchyRequests[0].enrich).toBe(false)
})

it('never reads a hierarchy for a room that is not a space', async () => {
  const h = harness()
  h.store.show({ accountId: ACCOUNT, roomId: SPACE, joined: true })
  await vi.waitFor(() =>
    expect(h.store.state.value.metadata.value).not.toBeNull(),
  )
  h.frame(['space_hierarchy'])
  h.timeline('m.space.child')
  h.reconnect()
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.hierarchyRequests).toHaveLength(0)
  expect(h.store.state.value.hierarchy.requested).toBe(false)
})

it('reads the hierarchy once the room turns out to be a space, enriching then', async () => {
  const h = harness({ hierarchy: () => hierarchyPage(rows(1)) })
  h.store.show({ accountId: ACCOUNT, roomId: SPACE, joined: true })
  h.store.show({ accountId: ACCOUNT, roomId: SPACE, joined: true, space: true })
  await vi.waitFor(() => expect(h.list()).not.toBeNull())
  expect(h.hierarchyRequests).toEqual([
    { roomId: SPACE, limit: 50, cursor: null, enrich: true },
  ])
})

it('loads more by cursor without enrich, de-duplicating by room ID', async () => {
  const h = harness({
    hierarchy: ({ cursor }) =>
      cursor === null
        ? hierarchyPage(rows(2), { total_children: 4, next_cursor: 'c1' })
        : // `!r-1` repeats: its link's order changed mid-walk.
          hierarchyPage(rows(3, 'r', 1), { total_children: 4 }),
  })
  showSpace(h)
  await settle(h, 1)
  h.store.loadMore()
  expect(h.store.state.value.hierarchy.loadingMore).toBe(true)
  await settle(h, 2)
  expect(h.list()?.children.map((row) => row.room_id)).toEqual([
    '!r-0:hs',
    '!r-1:hs',
    '!r-2:hs',
    '!r-3:hs',
  ])
  expect(h.list()?.nextCursor).toBeNull()
  expect(h.store.state.value.hierarchy.loadingMore).toBe(false)
  expect(h.hierarchyRequests[1]).toEqual({
    roomId: SPACE,
    limit: 50,
    cursor: 'c1',
    enrich: false,
  })
  // At the end, Load more asks nothing.
  h.store.loadMore()
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.hierarchyRequests).toHaveLength(2)
})

it('re-reads the loaded prefix on a space_hierarchy frame, without enrich', async () => {
  const h = harness({
    hierarchy: ({ cursor, limit }) =>
      cursor === null
        ? hierarchyPage(rows(Math.min(limit, 80)), {
            total_children: 80,
            next_cursor: limit >= 80 ? null : `after-${limit}`,
          })
        : hierarchyPage(rows(30, 'r', 50), { total_children: 80 }),
  })
  showSpace(h)
  await settle(h, 1)
  h.store.loadMore()
  await settle(h, 2)
  expect(h.list()?.children).toHaveLength(80)

  h.frame(['space_hierarchy'])
  await settle(h, 3)
  expect(h.hierarchyRequests[2]).toEqual({
    roomId: SPACE,
    limit: 80,
    cursor: null,
    enrich: false,
  })
  // 500 rows or fewer: the response is the whole list, cursor included.
  expect(h.list()?.children).toHaveLength(80)
  expect(h.list()?.nextCursor).toBeNull()
})

it('ignores hierarchy frames for another space or another account', async () => {
  const h = harness({ hierarchy: () => hierarchyPage(rows(1)) })
  showSpace(h)
  await settle(h, 1)
  h.frame(['space_hierarchy'], NEXT)
  h.frame(['space_hierarchy'], SPACE, OTHER)
  h.frame(['metadata'])
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.hierarchyRequests).toHaveLength(1)
})

it('re-reads on an m.space.child timeline event, which no crawl frame announces', async () => {
  const h = harness({ hierarchy: () => hierarchyPage(rows(1)) })
  showSpace(h)
  await settle(h, 1)
  h.timeline('m.room.message')
  h.timeline('m.space.child', NEXT)
  h.timeline('m.space.child')
  await settle(h, 2)
  expect(h.hierarchyRequests[1].enrich).toBe(false)
})

it('folds frames that arrive mid-read into exactly one more read', async () => {
  const h = harness({ hierarchy: () => hierarchyPage(rows(1)) })
  h.hold()
  showSpace(h)
  await vi.waitFor(() => expect(h.heldCount()).toBe(1))
  h.frame(['space_hierarchy'])
  h.frame(['space_hierarchy'])
  h.frame(['space_hierarchy'])
  h.release()
  await settle(h, 2)
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.hierarchyRequests).toHaveLength(2)
})

it('re-reads on reconnect without enrich', async () => {
  const h = harness({ hierarchy: () => hierarchyPage(rows(1)) })
  showSpace(h)
  await settle(h, 1)
  h.reconnect()
  await settle(h, 2)
  expect(h.hierarchyRequests[1]).toEqual({
    roomId: SPACE,
    limit: 50,
    cursor: null,
    enrich: false,
  })
})

it('drops a late page for a space the panel has left', async () => {
  const h = harness({
    hierarchy: ({ roomId }) =>
      hierarchyPage([hierarchyChild(`!child-of-${roomId.slice(1)}`)]),
  })
  h.hold()
  showSpace(h)
  await vi.waitFor(() => expect(h.heldCount()).toBe(1))
  showSpace(h, NEXT)
  await vi.waitFor(() => expect(h.heldCount()).toBe(2))
  h.release()
  await vi.waitFor(() => expect(h.list()).not.toBeNull())
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.list()?.children.map((row) => row.room_id)).toEqual([
    '!child-of-next:hs',
  ])
})

it('keeps enrich on the next read when the opening read failed', async () => {
  let fail = true
  const h = harness({ hierarchy: () => hierarchyPage(rows(1)) })
  server.use(
    http.get(
      `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/space/hierarchy`,
      ({ request }) => {
        h.hierarchyRequests.push({
          roomId: SPACE,
          limit: 50,
          cursor: null,
          enrich: new URL(request.url).searchParams.get('enrich') === 'true',
        })
        if (fail) {
          return HttpResponse.json(
            { error: { code: 'unavailable', message: 'offline' } },
            { status: 503 },
          )
        }
        return HttpResponse.json({ data: hierarchyPage(rows(1)) })
      },
    ),
  )
  showSpace(h)
  await vi.waitFor(() =>
    expect(h.store.state.value.hierarchy.error).toBe('offline'),
  )
  fail = false
  h.store.retry()
  await vi.waitFor(() => expect(h.list()).not.toBeNull())
  expect(h.hierarchyRequests.map((request) => request.enrich)).toEqual([
    true,
    true,
  ])
  expect(h.store.state.value.hierarchy.error).toBeNull()
  // Once the server has it, later re-reads go without.
  h.frame(['space_hierarchy'])
  await vi.waitFor(() => expect(h.hierarchyRequests).toHaveLength(3))
  expect(h.hierarchyRequests[2].enrich).toBe(false)
})

/** A hierarchy whose crawl status the test sets between reads. */
function crawlHarness(initial: Partial<SpaceDiscoveryDto>) {
  let discovery = spaceDiscovery(initial)
  const h = harness({
    hierarchy: () => hierarchyPage(rows(1), { discovery }),
  })
  return {
    h,
    setCrawl: (next: Partial<SpaceDiscoveryDto>) => {
      discovery = spaceDiscovery(next)
    },
  }
}

it('resumes a paused crawl with one enrich read, which prompts no other', async () => {
  const { h, setCrawl } = crawlHarness({ outcome: 'complete' })
  showSpace(h)
  await settle(h, 1)
  // A dropped crawl page, or a server restart: in progress, nothing running.
  setCrawl({ outcome: 'in_progress', state: 'idle' })
  h.frame(['space_hierarchy'])
  await settle(h, 3)
  expect(h.hierarchyRequests.map((request) => request.enrich)).toEqual([
    true,
    false,
    true,
  ])
  // The resume's own response still reads paused: it prompts nothing.
  await new Promise((resolve) => setTimeout(resolve, 30))
  expect(h.hierarchyRequests).toHaveLength(3)
})

it('resumes a paused crawl seen after a reconnect', async () => {
  const { h, setCrawl } = crawlHarness({ outcome: 'complete' })
  showSpace(h)
  await settle(h, 1)
  setCrawl({ outcome: 'in_progress', state: 'idle' })
  h.reconnect()
  await settle(h, 3)
  expect(h.hierarchyRequests.map((request) => request.enrich)).toEqual([
    true,
    false,
    true,
  ])
})

it('sends no resume read after a re-read that itself carried enrich', async () => {
  let reads = 0
  const enriched: boolean[] = []
  const h = harness()
  server.use(
    http.get(
      `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/space/hierarchy`,
      ({ request }) => {
        reads += 1
        enriched.push(
          new URL(request.url).searchParams.get('enrich') === 'true',
        )
        // The opening read fails, so the frame re-read carries its enrich;
        // the crawl then reads paused, which that enrich read already resumed.
        if (reads === 1) {
          return HttpResponse.json(
            { error: { code: 'unavailable', message: 'offline' } },
            { status: 503 },
          )
        }
        return HttpResponse.json({
          data: hierarchyPage(rows(1), {
            discovery: spaceDiscovery({
              outcome: 'in_progress',
              state: 'idle',
            }),
          }),
        })
      },
    ),
  )
  showSpace(h)
  await vi.waitFor(() =>
    expect(h.store.state.value.hierarchy.error).not.toBeNull(),
  )
  h.frame(['space_hierarchy'])
  await vi.waitFor(() => expect(h.list()).not.toBeNull())
  await new Promise((resolve) => setTimeout(resolve, 30))
  expect(enriched).toEqual([true, true])
})

it('sends no resume read when the frame re-read sees the next page queued', async () => {
  const { h, setCrawl } = crawlHarness({ outcome: 'complete' })
  showSpace(h)
  await settle(h, 1)
  setCrawl({ outcome: 'in_progress', state: 'queued', rooms_cached: 50 })
  h.frame(['space_hierarchy'])
  await settle(h, 2)
  await new Promise((resolve) => setTimeout(resolve, 30))
  expect(h.hierarchyRequests.map((request) => request.enrich)).toEqual([
    true,
    false,
  ])
})

it('never resumes a crawl in a space the account has not joined', async () => {
  const { h, setCrawl } = crawlHarness({ outcome: 'complete' })
  showSpace(h, SPACE, false)
  await settle(h, 1)
  setCrawl({ outcome: 'in_progress', state: 'idle' })
  h.frame(['space_hierarchy'])
  await settle(h, 2)
  await new Promise((resolve) => setTimeout(resolve, 30))
  expect(h.hierarchyRequests.every((request) => !request.enrich)).toBe(true)
})

it('retries a transient crawl failure on a timer, doubling when the server declines', async () => {
  vi.useFakeTimers({
    toFake: ['setTimeout', 'clearTimeout', 'Date'],
    shouldAdvanceTime: true,
  })
  // The server's backoff ends sooner than the floor: a fast client clock.
  const nextAttemptAt = Date.now() + 1_000
  const { h } = crawlHarness({
    outcome: 'failed',
    failure: 'timeout',
    next_attempt_at: nextAttemptAt,
  })
  showSpace(h)
  await settle(h, 1)
  // `vi.waitFor` advances fake timers while it polls, so each check below
  // leaves a second of margin either side of the deadline.
  vi.advanceTimersByTime(CRAWL_RETRY_MIN_MS - 1_000)
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.hierarchyRequests).toHaveLength(1)
  vi.advanceTimersByTime(2_000)
  await settle(h, 2)
  expect(h.hierarchyRequests[1].enrich).toBe(true)

  // Declined: the same `next_attempt_at`, so twice the delay, at least 10 s.
  vi.advanceTimersByTime(8_000)
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.hierarchyRequests).toHaveLength(2)
  vi.advanceTimersByTime(3_000)
  await settle(h, 3)
  // Then 20 s.
  vi.advanceTimersByTime(17_500)
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.hierarchyRequests).toHaveLength(3)
  vi.advanceTimersByTime(3_000)
  await settle(h, 4)
  expect(h.hierarchyRequests.every((request) => request.enrich)).toBe(true)
})

it('aims the retry timer at next_attempt_at, within the 30-minute bound', async () => {
  vi.useFakeTimers({
    toFake: ['setTimeout', 'clearTimeout', 'Date'],
    shouldAdvanceTime: true,
  })
  const { h } = crawlHarness({
    outcome: 'failed',
    failure: 'server_error',
    next_attempt_at: Date.now() + 3 * 3_600_000,
  })
  showSpace(h)
  await settle(h, 1)
  vi.advanceTimersByTime(CRAWL_RETRY_MAX_MS - 1_000)
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.hierarchyRequests).toHaveLength(1)
  vi.advanceTimersByTime(2_000)
  await settle(h, 2)
})

it('sets no retry timer for a conclusive failure', async () => {
  vi.useFakeTimers({
    toFake: ['setTimeout', 'clearTimeout', 'Date'],
    shouldAdvanceTime: true,
  })
  const { h } = crawlHarness({
    outcome: 'failed',
    failure: 'forbidden',
    next_attempt_at: Date.now() + 1_000,
  })
  showSpace(h)
  await settle(h, 1)
  vi.advanceTimersByTime(CRAWL_RETRY_MAX_MS + 1_000)
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.hierarchyRequests).toHaveLength(1)
})

it('cancels the retry timer when the panel closes or switches rooms', async () => {
  vi.useFakeTimers({
    toFake: ['setTimeout', 'clearTimeout', 'Date'],
    shouldAdvanceTime: true,
  })
  const { h } = crawlHarness({
    outcome: 'failed',
    failure: 'timeout',
    next_attempt_at: Date.now() + 1_000,
  })
  showSpace(h)
  await settle(h, 1)
  h.store.show(null)
  vi.advanceTimersByTime(CRAWL_RETRY_MAX_MS + 1_000)
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.hierarchyRequests).toHaveLength(1)
})

/**
 * A space of `total` children whose first-page reads return the first
 * `limit` of `children()` (by default, `rows(total)`), and whose cursor reads
 * return 300 rows after the cursor's position: big enough to pass 500 rows
 * in two pages.
 */
function bigSpace(total: number) {
  let children = rows(total)
  const h = harness({
    hierarchy: ({ cursor, limit }) => {
      const from = cursor === null ? 0 : Number(cursor.slice('at-'.length))
      const size = cursor === null ? limit : 300
      const page = children.slice(from, from + size)
      const end = from + page.length
      return hierarchyPage(page, {
        total_children: children.length,
        next_cursor: end < children.length ? `at-${end}` : null,
      })
    },
  })
  return {
    h,
    setChildren: (next: typeof children) => {
      children = next
    },
    children: () => children,
  }
}

async function loadPast500(h: Harness): Promise<void> {
  showSpace(h)
  await settle(h, 1)
  h.store.loadMore()
  await settle(h, 2)
  h.store.loadMore()
  await settle(h, 3)
  expect(h.list()?.children).toHaveLength(650)
  expect(h.list()?.nextCursor).toBe('at-650')
}

it('a frame re-read past 500 rows keeps the loaded tail and its cursor', async () => {
  const { h, setChildren, children } = bigSpace(1_000)
  await loadPast500(h)
  // A crawl changes summaries, not links: the first 500 come back as they were.
  setChildren(
    children().map((row, i) =>
      i === 3 ? { ...row, discovered: discoveredRoom() } : row,
    ),
  )
  h.frame(['space_hierarchy'])
  await settle(h, 4)
  expect(h.hierarchyRequests[3].limit).toBe(500)
  expect(h.list()?.children).toHaveLength(650)
  expect(h.list()?.children[3].discovered?.status).toBe('available')
  expect(h.list()?.nextCursor).toBe('at-650')
})

it('a frame re-read drops the tail when a link near the head pushes a row out', async () => {
  const { h, setChildren, children } = bigSpace(1_000)
  await loadPast500(h)
  // A child linked at the head pushes the old 500th row past the response.
  setChildren([hierarchyChild('!new:hs'), ...children()])
  h.frame(['space_hierarchy'])
  await settle(h, 4)
  // Collapsed to the response, with its cursor: no row vanished in between.
  expect(h.list()?.children).toHaveLength(500)
  expect(h.list()?.children[0].room_id).toBe('!new:hs')
  expect(h.list()?.nextCursor).toBe('at-500')
  h.store.loadMore()
  await settle(h, 5)
  expect(h.list()?.children.map((row) => row.room_id)).toContain('!r-499:hs')
})

it('a reconnect re-read past 500 rows always drops the tail', async () => {
  const { h } = bigSpace(1_000)
  await loadPast500(h)
  h.reconnect()
  await settle(h, 4)
  expect(h.list()?.children).toHaveLength(500)
  expect(h.list()?.nextCursor).toBe('at-500')
})

// ── One-room preview (decision 8) ───────────────────────────────────────────

const preview = (h: Harness, roomId = CHILD) => {
  const target = { accountId: ACCOUNT, roomId }
  h.store.showPreview(target)
  return target
}

it('previews one room from /discovery', async () => {
  const h = harness()
  h.store.showPreview({ accountId: ACCOUNT, roomId: CHILD })
  await vi.waitFor(() =>
    expect(h.store.preview.value.discovery.value?.room_id).toBe(CHILD),
  )
  expect(h.discoveryRequests).toEqual([CHILD])
})

it('re-reads the preview on a discovery frame for its room only', async () => {
  const h = harness()
  preview(h)
  await vi.waitFor(() => expect(h.discoveryRequests).toHaveLength(1))
  h.frame(['discovery'], NEXT)
  h.frame(['discovery'], CHILD, OTHER)
  h.frame(['metadata'], CHILD)
  h.frame(['discovery'], CHILD)
  await vi.waitFor(() => expect(h.discoveryRequests).toHaveLength(2))
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.discoveryRequests).toHaveLength(2)
})

it('while unsupported, re-reads on any space_hierarchy frame until an answer arrives', async () => {
  let answer: RoomDiscoveryDto = roomDiscovery(CHILD, {
    support: 'unsupported',
    discovered: null,
  })
  const h = harness({ discovery: () => answer })
  preview(h)
  await vi.waitFor(() =>
    expect(h.store.preview.value.discovery.value).not.toBeNull(),
  )
  // A space the client cannot predict: not the one the row came from.
  h.frame(['space_hierarchy'], '!some-other-space:hs')
  await vi.waitFor(() => expect(h.discoveryRequests).toHaveLength(2))

  // Any status is conclusive: a denial stops the frame re-reads too.
  answer = roomDiscovery(CHILD, {
    support: 'unsupported',
    discovered: discoveredRoom({ status: 'not_found', name: null }),
  })
  h.frame(['space_hierarchy'], '!some-other-space:hs')
  await vi.waitFor(() =>
    expect(h.store.preview.value.discovery.value?.discovered?.status).toBe(
      'not_found',
    ),
  )
  h.frame(['space_hierarchy'], '!some-other-space:hs')
  h.frame(['space_hierarchy'], SPACE)
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.discoveryRequests).toHaveLength(3)
})

it('re-reads an unsupported preview on hierarchy frames once its answer expires', async () => {
  const h = harness({
    discovery: () =>
      roomDiscovery(CHILD, {
        support: 'unsupported',
        discovered: discoveredRoom({ expires_at: Date.now() - 1 }),
      }),
  })
  preview(h)
  await vi.waitFor(() =>
    expect(h.store.preview.value.discovery.value).not.toBeNull(),
  )
  h.frame(['space_hierarchy'], SPACE)
  await vi.waitFor(() => expect(h.discoveryRequests).toHaveLength(2))
})

it('a supported preview ignores hierarchy frames', async () => {
  const h = harness({
    discovery: () => roomDiscovery(CHILD, { discovered: null }),
  })
  preview(h)
  await vi.waitFor(() =>
    expect(h.store.preview.value.discovery.value).not.toBeNull(),
  )
  h.frame(['space_hierarchy'], SPACE)
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.discoveryRequests).toHaveLength(1)
})

it('re-reads the preview on reconnect', async () => {
  const h = harness()
  preview(h)
  await vi.waitFor(() => expect(h.discoveryRequests).toHaveLength(1))
  h.reconnect()
  await vi.waitFor(() => expect(h.discoveryRequests).toHaveLength(2))
})

it('closing the preview stops it; closing another room’s does nothing', async () => {
  const h = harness()
  const first = preview(h)
  await vi.waitFor(() => expect(h.discoveryRequests).toHaveLength(1))
  // A second prompt takes the preview; the first prompt closing later must
  // not clear it.
  const second = preview(h, NEXT)
  h.store.closePreview(first)
  expect(h.store.preview.value.key).not.toBeNull()
  await vi.waitFor(() => expect(h.discoveryRequests).toHaveLength(2))
  h.store.closePreview(second)
  expect(h.store.preview.value.key).toBeNull()
  h.frame(['discovery'], NEXT)
  h.reconnect()
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.discoveryRequests).toHaveLength(2)
})

it('drops a late /discovery answer after the preview closed', async () => {
  let release: () => void = () => {}
  const held = new Promise<void>((resolve) => (release = resolve))
  const h = harness()
  server.use(
    http.get(
      `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/discovery`,
      async () => {
        h.discoveryRequests.push(CHILD)
        await held
        return HttpResponse.json({ data: roomDiscovery(CHILD) })
      },
    ),
  )
  const target = preview(h)
  await vi.waitFor(() => expect(h.discoveryRequests).toHaveLength(1))
  h.store.closePreview(target)
  release()
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.store.preview.value.discovery.value).toBeNull()
  expect(h.store.preview.value.key).toBeNull()
})

/**
 * A `/discovery` handler whose answers the test sets and can hold open, so a
 * frame can be delivered while a read is in flight.
 */
function heldDiscovery(h: Harness) {
  let answer: RoomDiscoveryDto | 'fail' = roomDiscovery(CHILD)
  const held: Array<() => void> = []
  let holding = false
  server.use(
    http.get(
      `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/discovery`,
      async () => {
        h.discoveryRequests.push(CHILD)
        // The answer as of the request: the server's snapshot is taken then.
        const current = answer
        if (holding) await new Promise<void>((resolve) => held.push(resolve))
        return current === 'fail'
          ? HttpResponse.json(
              { error: { code: 'unavailable', message: 'try later' } },
              { status: 503 },
            )
          : HttpResponse.json({ data: current })
      },
    ),
  )
  return {
    answer: (next: RoomDiscoveryDto | 'fail') => {
      answer = next
    },
    hold: () => {
      holding = true
    },
    release: () => {
      holding = false
      for (const resolve of held.splice(0)) resolve()
    },
    heldCount: () => held.length,
  }
}

it('judges a hierarchy frame that lands during the first read against its answer', async () => {
  const h = harness()
  const d = heldDiscovery(h)
  // The server takes its snapshot, schedules the fallback crawl, and the
  // crawl ends and publishes before this first answer reaches the browser.
  d.answer(roomDiscovery(CHILD, { support: 'unsupported', discovered: null }))
  d.hold()
  preview(h)
  await vi.waitFor(() => expect(d.heldCount()).toBe(1))
  h.frame(['space_hierarchy'], '!some-other-space:hs')
  d.answer(roomDiscovery(CHILD, { support: 'unsupported' }))
  d.release()
  await vi.waitFor(() =>
    expect(h.store.preview.value.discovery.value?.discovered?.status).toBe(
      'available',
    ),
  )
  expect(h.discoveryRequests).toHaveLength(2)
})

it('a mid-read hierarchy frame prompts nothing once the answer is conclusive', async () => {
  const h = harness()
  const d = heldDiscovery(h)
  d.answer(
    roomDiscovery(CHILD, {
      support: 'unsupported',
      discovered: discoveredRoom({ status: 'not_found', name: null }),
    }),
  )
  d.hold()
  preview(h)
  await vi.waitFor(() => expect(d.heldCount()).toBe(1))
  h.frame(['space_hierarchy'], SPACE)
  d.release()
  await vi.waitFor(() =>
    expect(h.store.preview.value.discovery.value).not.toBeNull(),
  )
  await new Promise((resolve) => setTimeout(resolve, 30))
  expect(h.discoveryRequests).toHaveLength(1)
})

it('retries a failed preview read on request, and only after a failure', async () => {
  const h = harness()
  const d = heldDiscovery(h)
  d.answer('fail')
  preview(h)
  await vi.waitFor(() =>
    expect(h.store.preview.value.discovery.error).toBe('try later'),
  )
  d.answer(roomDiscovery(CHILD))
  h.store.retryPreview()
  // Pending again at once, not still failed.
  expect(h.store.preview.value.discovery.error).toBeNull()
  await vi.waitFor(() =>
    expect(h.store.preview.value.discovery.value).not.toBeNull(),
  )
  expect(h.discoveryRequests).toHaveLength(2)
  h.store.retryPreview()
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.discoveryRequests).toHaveLength(2)
})

it('keeps the retry chain going when the timer’s own read fails', async () => {
  vi.useFakeTimers({
    toFake: ['setTimeout', 'clearTimeout', 'Date'],
    shouldAdvanceTime: true,
  })
  const failing = { now: false }
  const { h } = crawlHarness({
    outcome: 'failed',
    failure: 'timeout',
    next_attempt_at: Date.now() + 1_000,
  })
  server.use(
    http.get(
      `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/space/hierarchy`,
      ({ request }) => {
        if (!failing.now) return undefined
        h.hierarchyRequests.push({
          roomId: SPACE,
          limit: 50,
          cursor: null,
          enrich: new URL(request.url).searchParams.get('enrich') === 'true',
        })
        return HttpResponse.json(
          { error: { code: 'unavailable', message: 'offline' } },
          { status: 503 },
        )
      },
    ),
  )
  showSpace(h)
  await settle(h, 1)
  failing.now = true
  // The first timer (5 s) fires, and its read fails.
  vi.advanceTimersByTime(CRAWL_RETRY_MIN_MS + 1_000)
  await settle(h, 2)
  expect(h.store.state.value.hierarchy.error).toBe('offline')
  // Re-armed with backoff (10 s), not abandoned.
  vi.advanceTimersByTime(8_000)
  await new Promise((resolve) => setTimeout(resolve, 20))
  expect(h.hierarchyRequests).toHaveLength(2)
  vi.advanceTimersByTime(3_000)
  await settle(h, 3)
  expect(h.hierarchyRequests[2].enrich).toBe(true)
})

it('folded re-reads keep every reason they were asked for', () => {
  const retry = rereadFor('retry', 'frame', false)
  const frame = rereadFor('frame', 'frame', false)
  const timer = rereadFor('timer', 'frame', true)
  // A waiting Retry that absorbs a frame must still resume a paused crawl.
  expect(rereadPrompted(retry)).toBe(false)
  expect(rereadPrompted(mergeRereads(retry, frame))).toBe(true)
  expect(rereadPrompted(mergeRereads(frame, retry))).toBe(true)
  // And a frame that absorbs the timer's read still doubles the timer.
  const folded = mergeRereads(frame, timer)
  expect(folded.origins.has('timer')).toBe(true)
  expect(folded.enrich).toBe(true)
  expect(mergeRereads(rereadFor('open', 'reconnect', false), frame).mode).toBe(
    'reconnect',
  )
})

it('two prompts for one room share its preview until the last closes', async () => {
  const h = harness()
  const dialog = preview(h)
  const prompt = preview(h)
  await vi.waitFor(() =>
    expect(h.store.preview.value.discovery.value).not.toBeNull(),
  )
  // Opening the second prompt for the same room asks nothing more.
  expect(h.discoveryRequests).toHaveLength(1)
  h.store.closePreview(dialog)
  expect(h.store.preview.value.key).not.toBeNull()
  // The prompt still open keeps its frame refresh.
  h.frame(['discovery'], CHILD)
  await vi.waitFor(() => expect(h.discoveryRequests).toHaveLength(2))
  h.store.closePreview(prompt)
  expect(h.store.preview.value.key).toBeNull()
})
