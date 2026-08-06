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
import { createMediaService } from '../media/media-service'
import { createTimelineStore, inReplyToId, type EventDto } from './timeline'

const BASE_URL = 'http://axon.test'
const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'

// `relates_to` and `content` are free-form objects in the contract
// (generated as `Record<string, never>`), so overrides take them loosely
// and cast.
function event(
  id: string,
  ts: number,
  overrides: Partial<Omit<EventDto, 'relates_to' | 'content'>> & {
    relates_to?: unknown
    content?: unknown
  } = {},
): EventDto {
  return {
    account_id: ACCOUNT,
    event_id: id,
    room_id: ROOM,
    sender: '@alice:hs',
    origin_ts: ts,
    arrival_order: ts,
    type: 'm.room.message',
    body: `body of ${id}`,
    redacted: false,
    edited: false,
    edit_count: 0,
    ...overrides,
  } as EventDto
}

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function makeStore(threadRoot?: string) {
  const auth = {
    getToken: () => 'tok-test',
    onAuthFailure: () => {},
    LoginBootstrap: () => null,
  }
  const api = createApiClient(auth, BASE_URL)
  const media = createMediaService({ auth, baseUrl: BASE_URL })
  return createTimelineStore(api, media, ACCOUNT, ROOM, threadRoot)
}

const TIMELINE_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/timeline`
const SEND_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/send`
const EVENT_PATH = `${BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`

describe('createTimelineStore', () => {
  it('loads the newest page into ascending display order', async () => {
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: {
            // Server order: newest first.
            events: [event('$3', 300), event('$2', 200), event('$1', 100)],
            next_cursor: 'c1',
          },
        }),
      ),
    )

    const store = makeStore()
    await store.loadLatest()

    expect(store.loading.value).toBe(false)
    expect(store.events.value.map((e) => e.event_id)).toEqual([
      '$1',
      '$2',
      '$3',
    ])
    expect(store.atStart.value).toBe(false)
  })

  /// The head load reports its own outcome, because no signal it leaves behind
  /// distinguishes the three cases: `error` is store-wide (sends, redactions and
  /// the re-decryption retry write it too) and the parked-slice no-op touches
  /// nothing at all. Consumers gate read-state claims on this (ADR 0089/#136).
  describe('loadLatest reports what the head load did', () => {
    it('reports applied when it replaces an empty slice', async () => {
      server.use(
        http.get(TIMELINE_PATH, () =>
          HttpResponse.json({
            data: { events: [event('$1', 100)], next_cursor: null },
          }),
        ),
      )
      const store = makeStore()

      expect(await store.loadLatest()).toBe('applied')
      expect(store.atEnd.value).toBe(true)
    })

    it('reports failed when the fetch fails', async () => {
      server.use(
        http.get(TIMELINE_PATH, () => new HttpResponse(null, { status: 500 })),
      )
      const store = makeStore()

      expect(await store.loadLatest()).toBe('failed')
    })

    it('reports declined when it will not move a slice parked in history', async () => {
      server.use(
        http.get(TIMELINE_PATH, ({ request }) => {
          const atTs = new URL(request.url).searchParams.get('at_ts')
          return HttpResponse.json({
            data:
              atTs === null
                ? { events: [event('$head', 900)], next_cursor: 'c1' }
                : { events: [event('$old', 100)], next_cursor: 'c2' },
          })
        }),
      )
      const store = makeStore()
      await store.jumpTo(100)
      expect(store.atEnd.value).toBe(false)

      // The head shares nothing with the parked page, so `refreshHead` bails by
      // design (WCR-05) — and says so, rather than looking like a success.
      expect(await store.loadLatest()).toBe('declined')
      expect(store.events.value.map((e) => e.event_id)).toEqual(['$old'])
      expect(store.error.value).toBeNull()
    })

    it('reports applied when the head merges into an overlapping slice', async () => {
      server.use(
        http.get(TIMELINE_PATH, () =>
          HttpResponse.json({
            data: { events: [event('$1', 100)], next_cursor: 'c1' },
          }),
        ),
      )
      const store = makeStore()
      await store.loadLatest()

      expect(await store.loadLatest()).toBe('applied')
    })

    /// Losing a generation race is not a failure: the winner may have applied a
    /// perfectly good head. A caller that reads this as "couldn't verify" would
    /// strand whatever it was gating on (PR review on #136).
    it('reports superseded when a sibling load wins the race', async () => {
      let releaseFirst!: () => void
      const firstGate = new Promise<void>((resolve) => {
        releaseFirst = resolve
      })
      let calls = 0
      server.use(
        http.get(TIMELINE_PATH, async () => {
          calls += 1
          if (calls === 1) {
            await firstGate
            return HttpResponse.json({
              data: { events: [event('$stale', 100)], next_cursor: null },
            })
          }
          return HttpResponse.json({
            data: { events: [event('$fresh', 200)], next_cursor: null },
          })
        }),
      )
      const store = makeStore()

      const first = store.loadLatest() // stalled
      expect(await store.loadLatest()).toBe('applied') // supersedes it
      releaseFirst()

      expect(await first).toBe('superseded')
      // And the store is holding the winner's head, not nothing — which is why
      // the loser must not be treated as a failure.
      expect(store.events.value.map((e) => e.event_id)).toEqual(['$fresh'])
    })
  })

  it('loadOlder prepends and flags the room start', async () => {
    server.use(
      http.get(TIMELINE_PATH, ({ request }) => {
        const cursor = new URL(request.url).searchParams.get('cursor')
        if (cursor === null) {
          return HttpResponse.json({
            data: { events: [event('$3', 300)], next_cursor: 'c1' },
          })
        }
        expect(cursor).toBe('c1')
        return HttpResponse.json({
          data: {
            events: [event('$2', 200), event('$1', 100)],
            next_cursor: null,
          },
        })
      }),
    )

    const store = makeStore()
    await store.loadLatest()
    await store.loadOlder()

    expect(store.events.value.map((e) => e.event_id)).toEqual([
      '$1',
      '$2',
      '$3',
    ])
    expect(store.atStart.value).toBe(true)

    // At the start there is nothing further to fetch — no request happens.
    await store.loadOlder()
    expect(store.events.value).toHaveLength(3)
  })

  it('discards an in-flight older page when the slice is replaced (WCR-03)', async () => {
    let releaseOlder!: () => void
    const olderGate = new Promise<void>((resolve) => {
      releaseOlder = resolve
    })
    let headCalls = 0
    server.use(
      http.get(TIMELINE_PATH, async ({ request }) => {
        const cursor = new URL(request.url).searchParams.get('cursor')
        if (cursor === 'c1') {
          // The scroll-back page, stalled until after the gap-fill lands.
          await olderGate
          return HttpResponse.json({
            data: {
              events: [event('$2', 200), event('$1', 100)],
              next_cursor: 'c0',
            },
          })
        }
        headCalls += 1
        return HttpResponse.json({
          data:
            headCalls === 1
              ? { events: [event('$3', 300)], next_cursor: 'c1' }
              : { events: [event('$9', 900)], next_cursor: 'c8' },
        })
      }),
    )

    const store = makeStore()
    await store.loadLatest()
    const older = store.loadOlder() // in flight against cursor c1
    await store.loadLatest() // reconnect gap-fill replaces the slice
    releaseOlder()
    await older

    // The stale page fetched against the old cursor must not splice into the
    // replaced slice, and the cursor must stay the new head's.
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$9'])
    expect(store.loadingOlder.value).toBe(false)
    expect(store.atStart.value).toBe(false)
  })

  it('of two racing slice replacements, the newer request wins (WCR-03)', async () => {
    let releaseFirst!: () => void
    const firstGate = new Promise<void>((resolve) => {
      releaseFirst = resolve
    })
    let calls = 0
    server.use(
      http.get(TIMELINE_PATH, async () => {
        calls += 1
        if (calls === 1) {
          await firstGate
          return HttpResponse.json({
            data: { events: [event('$stale', 100)], next_cursor: null },
          })
        }
        return HttpResponse.json({
          data: { events: [event('$fresh', 200)], next_cursor: null },
        })
      }),
    )

    const store = makeStore()
    const first = store.loadLatest() // stalled
    await store.loadLatest() // completes first
    releaseFirst()
    await first

    // The older request's late response must not clobber the newer slice.
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$fresh'])
  })

  it('jumpTo passes at_ts and replaces the slice', async () => {
    let seenAtTs: string | null = null
    server.use(
      http.get(TIMELINE_PATH, ({ request }) => {
        seenAtTs = new URL(request.url).searchParams.get('at_ts')
        if (seenAtTs === null) {
          return HttpResponse.json({
            data: { events: [event('$9', 900)], next_cursor: 'c9' },
          })
        }
        return HttpResponse.json({
          data: {
            events: [event('$2', 200), event('$1', 100)],
            next_cursor: null,
          },
        })
      }),
    )

    const store = makeStore()
    await store.loadLatest()
    await store.jumpTo(250)

    expect(seenAtTs).toBe('250')
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$1', '$2'])
  })

  it('jumpToDate walks older pages until the page reaches the day start', async () => {
    const requests: string[] = []
    server.use(
      http.get(TIMELINE_PATH, ({ request }) => {
        const params = new URL(request.url).searchParams
        requests.push(params.toString())
        if (params.get('at_ts') === '1000') {
          return HttpResponse.json({
            data: {
              events: [event('$late2', 900), event('$late1', 800)],
              next_cursor: 'older',
            },
          })
        }
        if (params.get('cursor') === 'older') {
          return HttpResponse.json({
            data: {
              events: [event('$early', 150), event('$before', 90)],
              next_cursor: 'before',
            },
          })
        }
        return HttpResponse.json({
          data: { events: [], next_cursor: null },
        })
      }),
    )

    const store = makeStore()
    await store.jumpToDate(100, 1000)

    expect(requests).toEqual(['limit=50&at_ts=1000', 'limit=50&cursor=older'])
    expect(store.events.value.map((e) => e.event_id)).toEqual([
      '$before',
      '$early',
    ])
    expect(store.atEnd.value).toBe(false)
    expect(store.atStart.value).toBe(false)
  })

  it('jumpToDate does not stop on hidden thread members in the room timeline', async () => {
    const requests: string[] = []
    server.use(
      http.get(TIMELINE_PATH, ({ request }) => {
        const params = new URL(request.url).searchParams
        requests.push(params.toString())
        if (params.get('at_ts') === '1000') {
          return HttpResponse.json({
            data: {
              events: [
                event('$visible-late', 800),
                event('$hidden-thread-on-day', 90, {
                  relates_to: { rel_type: 'm.thread', event_id: '$root' },
                }),
              ],
              next_cursor: 'older',
            },
          })
        }
        if (params.get('cursor') === 'older') {
          return HttpResponse.json({
            data: {
              events: [event('$visible-on-day', 150), event('$before-day', 90)],
              next_cursor: 'before',
            },
          })
        }
        return HttpResponse.json({
          data: { events: [], next_cursor: null },
        })
      }),
    )

    const store = makeStore()
    await store.jumpToDate(100, 1000)

    expect(requests).toEqual(['limit=50&at_ts=1000', 'limit=50&cursor=older'])
    expect(store.events.value.map((e) => e.event_id)).toEqual([
      '$before-day',
      '$visible-on-day',
    ])
  })

  describe('forward paging after a jump (loadNewer)', () => {
    /**
     * A timeline handler with the real endpoint's paging semantics: pages are
     * newest-first, `at_ts` bounds them from above, `limit` sizes them. Good
     * enough for `loadNewer`'s probes; `cursor` is not modeled (the probes
     * never send one).
     */
    function pagedTimeline(all: ReturnType<typeof event>[]) {
      return http.get(TIMELINE_PATH, ({ request }) => {
        const params = new URL(request.url).searchParams
        const limit = Number(params.get('limit') ?? 50)
        const atTs = params.get('at_ts')
        let pool = [...all].sort((a, b) => b.origin_ts - a.origin_ts)
        if (atTs !== null) {
          pool = pool.filter((e) => e.origin_ts <= Number(atTs))
        }
        return HttpResponse.json({
          data: { events: pool.slice(0, limit), next_cursor: null },
        })
      })
    }

    it('jumpTo clears atEnd; loadNewer appends the contiguous newer run', async () => {
      const all = [event('$1', 100), event('$2', 200), event('$3', 300)]
      server.use(pagedTimeline(all))
      const store = makeStore()

      await store.loadLatest()
      expect(store.atEnd.value).toBe(true)

      await store.jumpTo(150)
      expect(store.atEnd.value).toBe(false)
      expect(store.events.value.map((e) => e.event_id)).toEqual(['$1'])

      await store.loadNewer()
      expect(store.events.value.map((e) => e.event_id)).toEqual([
        '$1',
        '$2',
        '$3',
      ])
      // The run reached the present, but only an *empty* probe proves it.
      expect(store.atEnd.value).toBe(false)
      await store.loadNewer()
      expect(store.atEnd.value).toBe(true)
    })

    it('a failed head refetch after a jump leaves atEnd false', async () => {
      const all = [event('$1', 100), event('$2', 200), event('$3', 300)]
      server.use(pagedTimeline(all))
      const store = makeStore()

      await store.jumpTo(150)
      expect(store.atEnd.value).toBe(false)
      expect(store.events.value.map((e) => e.event_id)).toEqual(['$1'])

      // The reconnect gap-fill hits a briefly-unreachable server: `atEnd`
      // must not latch true before the head fetch settles, or live frames
      // would splice the present onto the parked historical slice.
      server.use(
        http.get(TIMELINE_PATH, () =>
          HttpResponse.json({ error: 'boom' }, { status: 500 }),
        ),
      )
      await store.loadLatest()
      expect(store.atEnd.value).toBe(false)

      store.ingestLive(event('$4', 400))
      expect(store.events.value.map((e) => e.event_id)).toEqual(['$1'])
    })

    it('shrinks the probe window past a full page that would hide a gap', async () => {
      // 120 events one ms apart above the pivot: the first probes return a
      // full page floating entirely above ts=1000, which must narrow the
      // window rather than splice a hole under the page (50 < 120).
      const all = [
        event('$pivot', 1000),
        ...Array.from({ length: 120 }, (_, i) => event(`$n${i}`, 1001 + i)),
      ]
      server.use(pagedTimeline(all))
      const store = makeStore()

      await store.jumpTo(1000)
      expect(store.events.value.map((e) => e.event_id)).toEqual(['$pivot'])

      await store.loadNewer()
      const ids = store.events.value.map((e) => e.event_id)
      // Contiguity: whatever arrived starts directly above the pivot.
      expect(ids[0]).toBe('$pivot')
      expect(ids[1]).toBe('$n0')
      expect(ids.length).toBeGreaterThan(1)
      // No holes anywhere in the appended run.
      const timestamps = store.events.value.map((e) => e.origin_ts)
      for (let i = 1; i < timestamps.length; i += 1) {
        expect(timestamps[i]).toBe(timestamps[i - 1] + 1)
      }
    })

    it('crosses an origin_ts tie at the loaded tail without dropping events', async () => {
      // $b shares $a's millisecond and is not loaded. A strict `> pivot`
      // bound would read the probe as "nothing newer" at pivot=$a, flip
      // atEnd, and strand $b and $c — ties are routine in a send burst.
      const all = [event('$a', 200), event('$b', 200), event('$c', 300)]
      server.use(
        http.get(TIMELINE_PATH, ({ request }) => {
          const atTs = new URL(request.url).searchParams.get('at_ts')
          if (atTs === '200') {
            // The jump page cuts inside the tie group (a limit boundary).
            return HttpResponse.json({
              data: { events: [event('$a', 200)], next_cursor: null },
            })
          }
          let pool = [...all].sort((x, y) => y.origin_ts - x.origin_ts)
          if (atTs !== null) {
            pool = pool.filter((e) => e.origin_ts <= Number(atTs))
          }
          return HttpResponse.json({
            data: { events: pool, next_cursor: null },
          })
        }),
      )
      const store = makeStore()
      await store.jumpTo(200)
      expect(store.events.value.map((e) => e.event_id)).toEqual(['$a'])

      await store.loadNewer()
      expect(store.events.value.map((e) => e.event_id)).toEqual([
        '$a',
        '$b',
        '$c',
      ])
      expect(store.atEnd.value).toBe(false)
    })

    it('crosses a quiet gap under a burst wider than one page', async () => {
      // Nothing for 4 seconds above the pivot, then 60 events in one burst:
      // no single at_ts window both overlaps the tail and returns a non-full
      // page of the burst, so the probe must stitch an overlap page to a
      // floating page whose bottom meets the proven-covered range.
      const all = [
        event('$pivot', 1000),
        ...Array.from({ length: 60 }, (_, i) => event(`$b${i}`, 5000 + i)),
      ]
      server.use(pagedTimeline(all))
      const store = makeStore()

      await store.jumpTo(1000)
      expect(store.events.value.map((e) => e.event_id)).toEqual(['$pivot'])

      await store.loadNewer()
      expect(store.events.value).toHaveLength(61)
      expect(store.events.value.at(-1)?.event_id).toBe('$b59')
      const timestamps = store.events.value.map((e) => e.origin_ts)
      for (let i = 2; i < timestamps.length; i += 1) {
        expect(timestamps[i]).toBe(timestamps[i - 1] + 1)
      }

      await store.loadNewer()
      expect(store.atEnd.value).toBe(true)
    })

    it('suppresses live appends while jumped, resumes at the end', async () => {
      const all = [event('$1', 100), event('$2', 200)]
      server.use(pagedTimeline(all))
      const store = makeStore()

      await store.jumpTo(100)
      expect(store.events.value.map((e) => e.event_id)).toEqual(['$1'])

      // A frame from the present must not splice onto a slice parked at 100…
      store.ingestLive(event('$live', 5000))
      expect(store.events.value.map((e) => e.event_id)).toEqual(['$1'])
      // …but a replacement of a *loaded* row still applies.
      store.ingestLive(event('$1', 100, { body: 'edited' }))
      expect(store.events.value[0].body).toBe('edited')

      // Catch up: the run appends, a second (empty) probe flips atEnd,
      // and live appends work again.
      await store.loadNewer()
      await store.loadNewer()
      expect(store.atEnd.value).toBe(true)
      store.ingestLive(event('$live', 5000))
      expect(store.events.value.map((e) => e.event_id)).toEqual([
        '$1',
        '$2',
        '$live',
      ])
    })

    it('keeps local echoes at the tail across a loadNewer append', async () => {
      const all = [event('$1', 100), event('$2', 200)]
      server.use(
        pagedTimeline(all),
        http.post(SEND_PATH, () =>
          HttpResponse.json(
            { error: { code: 'x', message: 'nope' } },
            { status: 500 },
          ),
        ),
      )
      const store = makeStore()
      await store.jumpTo(100)
      // A failed send leaves a retryable echo at the tail (WCR rule); it must
      // stay the last row when history grows underneath it.
      await store.send('draft', { senderId: '@alice:hs' })
      await store.loadNewer()
      const ids = store.events.value.map((e) => e.event_id)
      expect(ids.slice(0, 2)).toEqual(['$1', '$2'])
      expect(store.events.value.at(-1)?.localEcho?.status).toBe('failed')
    })
  })

  it('loadOlder reports a landed page, an empty one included', async () => {
    let calls = 0
    server.use(
      http.get(TIMELINE_PATH, () => {
        calls += 1
        if (calls === 1) {
          return HttpResponse.json({
            data: { events: [event('$2', 200)], next_cursor: 'c1' },
          })
        }
        if (calls === 2) {
          // Landed, cursor moved on, nothing in it for this view.
          return HttpResponse.json({ data: { events: [], next_cursor: 'c2' } })
        }
        return HttpResponse.json({
          data: { events: [event('$1', 100)], next_cursor: null },
        })
      }),
    )
    const store = makeStore()
    await store.loadLatest()

    // An empty page is progress: the cursor moved, so paging can continue.
    expect(await store.loadOlder()).toBe(true)
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$2'])

    expect(await store.loadOlder()).toBe(true)
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$1', '$2'])

    // Out of history: nothing was fetched, so the caller must stop.
    expect(store.atStart.value).toBe(true)
    expect(await store.loadOlder()).toBe(false)
  })

  it('loadOlder reports a failed fetch and clears its in-flight flag', async () => {
    let calls = 0
    server.use(
      http.get(TIMELINE_PATH, () => {
        calls += 1
        return calls === 1
          ? HttpResponse.json({
              data: { events: [event('$2', 200)], next_cursor: 'c1' },
            })
          : HttpResponse.error()
      }),
    )
    const store = makeStore()
    await store.loadLatest()

    expect(await store.loadOlder()).toBe(false)
    // A latched flag would make every later page a silent no-op.
    expect(store.loadingOlder.value).toBe(false)
  })

  describe('retained-slice bound', () => {
    /** History paged 50 at a time by an index cursor, newest page first. */
    function cursorPagedHistory(total: number) {
      // Ascending ts, so page 0 is the newest 50 and the cursor walks older.
      const all = Array.from({ length: total }, (_, i) =>
        event(`$e${i}`, (i + 1) * 100),
      )
      return http.get(TIMELINE_PATH, ({ request }) => {
        const cursor = new URL(request.url).searchParams.get('cursor')
        const page = cursor === null ? 0 : Number(cursor)
        const end = total - page * 50
        const start = Math.max(0, end - 50)
        return HttpResponse.json({
          data: {
            events: all.slice(start, end).reverse(),
            next_cursor: start === 0 ? null : String(page + 1),
          },
        })
      })
    }

    it('drops the newest end once a scroll-back passes the cap', async () => {
      server.use(cursorPagedHistory(1000))
      const store = makeStore()
      await store.loadLatest()
      expect(store.atEnd.value).toBe(true)

      for (let page = 0; page < 15; page += 1) {
        await store.loadOlder()
      }

      // 16 pages fetched, 12 pages' worth kept — from the oldest end, which
      // is where the reader is.
      expect(store.events.value).toHaveLength(600)
      expect(store.events.value[0].event_id).toBe('$e200')
      expect(store.events.value.at(-1)?.event_id).toBe('$e799')
      // The slice no longer runs to the room's tail.
      expect(store.atEnd.value).toBe(false)
    })

    it('a trimmed slice stops taking live events and walks back forward', async () => {
      server.use(cursorPagedHistory(1000))
      const store = makeStore()
      await store.loadLatest()
      for (let page = 0; page < 15; page += 1) {
        await store.loadOlder()
      }
      expect(store.atEnd.value).toBe(false)

      // Parked in history: a live frame must not splice the present onto the
      // past — exactly the rule a date jump already relies on.
      store.ingestLive(event('$live', 500_000))
      expect(store.events.value.at(-1)?.event_id).toBe('$e799')

      // Coming back down walks forward through what the trim dropped.
      await store.loadNewer()
      expect(store.events.value.at(-1)?.event_id).not.toBe('$e799')
    })

    it('never trims a pending local echo off the tail', async () => {
      server.use(
        cursorPagedHistory(1000),
        http.post(SEND_PATH, () =>
          HttpResponse.json({ data: { event_id: '$sent' } }),
        ),
        // The reconcile misses, so the echo stays pending across the trim.
        http.get(EVENT_PATH, () => new HttpResponse(null, { status: 404 })),
      )
      const store = makeStore()
      await store.loadLatest()
      await store.send('unsent work', { senderId: '@alice:hs' })
      expect(store.events.value.at(-1)?.localEcho?.status).toBe('pending')

      for (let page = 0; page < 15; page += 1) {
        await store.loadOlder()
      }

      const echoes = store.events.value.filter((e) => e.localEcho !== undefined)
      expect(echoes).toHaveLength(1)
      expect(echoes[0].localEcho?.body).toBe('unsent work')
      // It rides at the tail, after the retained history.
      expect(store.events.value.at(-1)?.localEcho?.status).toBe('pending')
    })

    it('a reconnect leaves a trimmed slice parked instead of teleporting', async () => {
      server.use(cursorPagedHistory(1000))
      const store = makeStore()
      await store.loadLatest()
      for (let page = 0; page < 15; page += 1) {
        await store.loadOlder()
      }
      const parked = store.events.value.map((e) => e.event_id)

      // The gap-fill head shares nothing with a slice this far back. Without
      // the parked check it replaced the slice wholesale, yanking a reader
      // hundreds of events forward mid-scroll-back.
      await store.loadLatest()

      expect(store.events.value.map((e) => e.event_id)).toEqual(parked)
      expect(store.atEnd.value).toBe(false)
    })

    it('keeps a pending echo when a disjoint head replaces the slice', async () => {
      let head = 0
      server.use(
        http.get(TIMELINE_PATH, () => {
          head += 1
          return HttpResponse.json({
            data:
              head === 1
                ? { events: [event('$3', 300)], next_cursor: 'c1' }
                : { events: [event('$9', 900)], next_cursor: 'c8' },
          })
        }),
        http.post(SEND_PATH, () =>
          HttpResponse.json(
            { error: { code: 'bad_gateway', message: 'homeserver down' } },
            { status: 502 },
          ),
        ),
      )
      const store = makeStore()
      await store.loadLatest()
      expect(await store.send('unsent work', { senderId: '@alice:hs' })).toBe(
        false,
      )

      // The head shares nothing with the loaded slice, so the *history* is
      // replaced — but an unsent send is not history, and no server page can
      // speak to it. Dropping it here is how a re-entered room used to lose
      // the user's failed message.
      await store.loadLatest()

      expect(store.events.value.map((e) => e.event_id)).toEqual([
        '$9',
        expect.stringMatching(/^local:/) as unknown as string,
      ])
      expect(store.events.value[1].localEcho?.status).toBe('failed')
      expect(store.atEnd.value).toBe(true)
    })

    it('drops an echo the disjoint head turns out to confirm', async () => {
      let head = 0
      server.use(
        http.get(TIMELINE_PATH, () => {
          head += 1
          return HttpResponse.json({
            data:
              head === 1
                ? { events: [event('$3', 300)], next_cursor: 'c1' }
                : {
                    events: [
                      event('$9', 900, {
                        sender: '@alice:hs',
                        body: 'landed after all',
                      }),
                    ],
                    next_cursor: 'c8',
                  },
          })
        }),
        // The send reached the server and the response never came back, so
        // the echo is still *pending* — the only status a confirming page may
        // silently drop. A `failed` echo belongs to the user, to retry or
        // discard, and the merge leaves it alone by the same rule.
        http.post(SEND_PATH, () => new Promise<Response>(() => {})),
      )
      const store = makeStore()
      await store.loadLatest()
      const inFlight = store.send('landed after all', { senderId: '@alice:hs' })
      expect(store.events.value.at(-1)?.localEcho?.status).toBe('pending')

      await store.loadLatest()
      void inFlight

      // One row, the confirmed one — not the echo beside its own event.
      expect(store.events.value.map((e) => e.event_id)).toEqual(['$9'])
    })

    it('still gap-fills a slice that claimed to be at the tail', async () => {
      let head = 0
      server.use(
        http.get(TIMELINE_PATH, () => {
          head += 1
          return HttpResponse.json({
            data:
              head === 1
                ? { events: [event('$3', 300)], next_cursor: 'c1' }
                : { events: [event('$9', 900)], next_cursor: 'c8' },
          })
        }),
      )
      const store = makeStore()
      await store.loadLatest()
      expect(store.atEnd.value).toBe(true)

      // A real gap opened while offline: the slice said it was at the tail
      // and is not, so the head must still replace it.
      await store.loadLatest()
      expect(store.events.value.map((e) => e.event_id)).toEqual(['$9'])
      expect(store.atEnd.value).toBe(true)
    })
  })

  it('resolves reply targets locally, then by fetch, caching misses', async () => {
    const reply = event('$reply', 300, {
      relates_to: { 'm.in_reply_to': { event_id: '$1' } },
    })
    const farReply = event('$far', 400, {
      relates_to: { 'm.in_reply_to': { event_id: '$outside' } },
    })
    let fetchedIds: string[] = []
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: {
            events: [farReply, reply, event('$1', 100)],
            next_cursor: null,
          },
        }),
      ),
      http.get(
        `${BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
        ({ params }) => {
          fetchedIds.push(params.eventId as string)
          return HttpResponse.json({ data: event('$outside', 50) })
        },
      ),
    )

    const store = makeStore()
    await store.loadLatest()

    // $1 resolved from the loaded slice without a fetch.
    expect(store.replyTargets.value.get('$1')?.event_id).toBe('$1')
    await vi.waitFor(() =>
      expect(store.replyTargets.value.get('$outside')?.origin_ts).toBe(50),
    )
    expect(fetchedIds).toEqual(['$outside'])

    // Reloading does not refetch already-resolved targets.
    fetchedIds = []
    await store.loadLatest()
    await new Promise((resolve) => setTimeout(resolve, 5))
    expect(fetchedIds).toEqual([])
  })

  it('surfaces page-fetch errors', async () => {
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json(
          { error: { code: 'not_found', message: 'no such room' } },
          { status: 404 },
        ),
      ),
    )
    const store = makeStore()
    await store.loadLatest()
    expect(store.error.value).toBe('no such room')
    expect(store.loading.value).toBe(false)
  })
})

describe('ingestLive', () => {
  it('appends an unseen live event', () => {
    const store = makeStore()
    store.ingestLive(event('$live', 500))
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$live'])
  })

  it('ignores events for another room or account', () => {
    const store = makeStore()
    store.ingestLive(event('$a', 1, { room_id: '!elsewhere:hs' }))
    store.ingestLive(
      event('$b', 2, { account_id: '6b53f7f0-0000-4000-8000-999999999999' }),
    )
    expect(store.events.value).toHaveLength(0)
  })

  it('replaces a known id in place (a live edit)', async () => {
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: { events: [event('$1', 100)], next_cursor: null },
        }),
      ),
    )
    const store = makeStore()
    await store.loadLatest()

    store.ingestLive(event('$1', 100, { body: 'edited', edited: true }))
    expect(store.events.value).toHaveLength(1)
    expect(store.events.value[0].body).toBe('edited')
  })

  it('refreshes a live reaction target instead of appending the raw relation', async () => {
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: { events: [event('$1', 100)], next_cursor: null },
        }),
      ),
      http.get(EVENT_PATH, ({ params }) => {
        expect(params.eventId).toBe('$1')
        return HttpResponse.json({
          data: event('$1', 100, {
            reactions: {
              '🔥': {
                count: 1,
                me: false,
                senders: ['@bob:hs'],
                my_event_ids: [],
              },
            },
          }),
        })
      }),
    )
    const store = makeStore()
    await store.loadLatest()

    store.ingestLive(
      event('$rx', 101, {
        sender: '@bob:hs',
        type: 'm.reaction',
        body: null,
        relates_to: {
          rel_type: 'm.annotation',
          event_id: '$1',
          key: '🔥',
        },
      }),
    )

    await vi.waitFor(() =>
      expect(store.events.value[0].reactions?.['🔥']?.count).toBe(1),
    )
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$1'])
  })

  it('refreshes a loaded reaction target when a live redaction withdraws my reaction', async () => {
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: {
            events: [
              event('$1', 100, {
                reactions: {
                  '🔥': {
                    count: 1,
                    me: true,
                    senders: ['@alice:hs'],
                    my_event_ids: ['$rx'],
                  },
                },
              }),
            ],
            next_cursor: null,
          },
        }),
      ),
      http.get(EVENT_PATH, ({ params }) => {
        expect(params.eventId).toBe('$1')
        return HttpResponse.json({
          data: event('$1', 100, { reactions: null }),
        })
      }),
    )
    const store = makeStore()
    await store.loadLatest()

    store.ingestLive(
      event('$redact', 101, {
        type: 'm.room.redaction',
        body: null,
        content: { redacts: '$rx' },
      }),
    )

    await vi.waitFor(() => expect(store.events.value[0].reactions).toBeNull())
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$1'])
  })

  it('refreshes a live reaction target when a later live redaction withdraws it', async () => {
    let fetches = 0
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: { events: [event('$1', 100)], next_cursor: null },
        }),
      ),
      http.get(EVENT_PATH, ({ params }) => {
        expect(params.eventId).toBe('$1')
        fetches += 1
        return HttpResponse.json({
          data:
            fetches === 1
              ? event('$1', 100, {
                  reactions: {
                    '🔥': {
                      count: 1,
                      me: false,
                      senders: ['@bob:hs'],
                      my_event_ids: [],
                    },
                  },
                })
              : event('$1', 100, { reactions: null }),
        })
      }),
    )
    const store = makeStore()
    await store.loadLatest()

    store.ingestLive(
      event('$rx', 101, {
        sender: '@bob:hs',
        type: 'm.reaction',
        body: null,
        relates_to: {
          rel_type: 'm.annotation',
          event_id: '$1',
          key: '🔥',
        },
      }),
    )
    await vi.waitFor(() =>
      expect(store.events.value[0].reactions?.['🔥']?.count).toBe(1),
    )

    store.ingestLive(
      event('$redact', 102, {
        sender: '@bob:hs',
        type: 'm.room.redaction',
        body: null,
        content: { redacts: '$rx' },
      }),
    )

    await vi.waitFor(() => expect(store.events.value[0].reactions).toBeNull())
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$1'])
  })

  it('refreshes a live replacement target instead of appending the raw edit', async () => {
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: { events: [event('$1', 100)], next_cursor: null },
        }),
      ),
      http.get(EVENT_PATH, ({ params }) => {
        expect(params.eventId).toBe('$1')
        return HttpResponse.json({
          data: event('$1', 100, { body: 'edited', edited: true }),
        })
      }),
    )
    const store = makeStore()
    await store.loadLatest()

    store.ingestLive(
      event('$edit', 101, {
        body: 'edited',
        relates_to: {
          rel_type: 'm.replace',
          event_id: '$1',
        },
      }),
    )

    await vi.waitFor(() => expect(store.events.value[0].body).toBe('edited'))
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$1'])
  })

  it('drops a pending local echo the live event confirms (own-send race)', async () => {
    server.use(
      http.post(SEND_PATH, () =>
        HttpResponse.json({ data: { event_id: '$real' } }),
      ),
      // The reconcile GET misses, so the echo is still pending when the live
      // frame for the same send arrives.
      http.get(EVENT_PATH, () => new HttpResponse(null, { status: 404 })),
    )
    const store = makeStore()
    await store.send('hello', { senderId: '@alice:hs' })
    expect(store.events.value).toHaveLength(1)
    expect(store.events.value[0].localEcho?.status).toBe('pending')

    store.ingestLive(
      event('$real', 999, { sender: '@alice:hs', body: 'hello' }),
    )
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$real'])
    expect(store.events.value[0].localEcho).toBeUndefined()
  })

  it('one frame confirms only one of two identical pending echoes (WCR-07)', async () => {
    let sends = 0
    server.use(
      http.post(SEND_PATH, () => {
        sends += 1
        return HttpResponse.json({ data: { event_id: `$s${sends}` } })
      }),
      // Reconcile misses, so both echoes stay pending — the frames are the
      // only confirmation path, as when the broadcast beats the reconcile.
      http.get(EVENT_PATH, () => new HttpResponse(null, { status: 404 })),
    )
    const store = makeStore()
    await store.send('ok', { senderId: '@alice:hs' })
    await store.send('ok', { senderId: '@alice:hs' })
    expect(store.events.value).toHaveLength(2)

    // The first frame must not sweep away both same-body echoes: the bus is
    // lossy, and the second send still needs its own confirmation.
    store.ingestLive(event('$s1', 900, { sender: '@alice:hs', body: 'ok' }))
    expect(store.events.value).toHaveLength(2)
    // The confirmation replaces the echo *in place* (ADR 0081). It used to be
    // dropped and appended, which put the first send behind the second — wrong
    // chronologically here, and order-destroying for a multi-image batch.
    expect(store.events.value[0].event_id).toBe('$s1')
    expect(store.events.value[1].event_id).toMatch(/^local:/)
    expect(store.events.value[1].localEcho?.status).toBe('pending')

    store.ingestLive(event('$s2', 901, { sender: '@alice:hs', body: 'ok' }))
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$s1', '$s2'])
  })

  it('reconcile drops the echo instead of duplicating an event the frame already appended (WCR-07)', async () => {
    let releaseReconcile!: () => void
    const reconcileGate = new Promise<void>((resolve) => {
      releaseReconcile = resolve
    })
    server.use(
      http.post(SEND_PATH, () =>
        HttpResponse.json({ data: { event_id: '$real' } }),
      ),
      http.get(EVENT_PATH, async () => {
        await reconcileGate
        return HttpResponse.json({
          data: event('$real', 999, { sender: '@alice:hs', body: 'hello' }),
        })
      }),
    )
    const store = makeStore()
    // No senderId (rooms not loaded yet): the echo's sender is '', so the
    // live frame's confirmsEcho match misses and the frame appends $real
    // beside the still-pending echo.
    const sending = store.send('hello')
    await Promise.resolve()
    store.ingestLive(
      event('$real', 999, { sender: '@alice:hs', body: 'hello' }),
    )
    expect(store.events.value).toHaveLength(2)

    releaseReconcile()
    await sending

    // The reconcile must not turn the echo into a second $real row.
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$real'])
  })

  it('on a thread timeline, takes only its own thread replies', () => {
    const store = makeStore('$root')
    store.ingestLive(
      event('$reply', 1, {
        relates_to: { rel_type: 'm.thread', event_id: '$root' },
      }),
    )
    store.ingestLive(
      event('$other', 2, {
        relates_to: { rel_type: 'm.thread', event_id: '$otherroot' },
      }),
    )
    store.ingestLive(event('$plain', 3))
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$reply'])
  })
})

describe('inReplyToId', () => {
  it('extracts the reply target and tolerates junk', () => {
    expect(
      inReplyToId(
        event('$r', 1, { relates_to: { 'm.in_reply_to': { event_id: '$t' } } }),
      ),
    ).toBe('$t')
    expect(inReplyToId(event('$r', 1))).toBeNull()
    expect(
      inReplyToId(event('$r', 1, { relates_to: { rel_type: 'm.thread' } })),
    ).toBeNull()
    expect(
      inReplyToId(
        event('$r', 1, {
          relates_to: {
            rel_type: 'm.thread',
            event_id: '$root',
            is_falling_back: true,
            'm.in_reply_to': {
              event_id: '$root',
            },
          },
        }),
      ),
    ).toBeNull()
    expect(
      inReplyToId(
        event('$r', 1, {
          relates_to: {
            rel_type: 'm.thread',
            event_id: '$root',
            is_falling_back: false,
            'm.in_reply_to': {
              event_id: '$reply',
            },
          },
        }),
      ),
    ).toBe('$reply')
    expect(
      inReplyToId(
        event('$r', 1, { relates_to: { 'm.in_reply_to': { event_id: 42 } } }),
      ),
    ).toBeNull()
  })
})

/**
 * `loading` means "there is nothing to show yet", not "a request is in
 * flight". RoomPage swaps the whole timeline for "Loading timeline…" while it
 * is true, and the reconnect gap-fill (ADR 0061) calls `loadLatest` on an
 * already-loaded slice — so a socket that drops and reopens blanked the
 * timeline each time. On a phone the sidebar is hidden and the timeline is the
 * entire view, which made every reconnect look like a page reload.
 */
describe('a reload over an existing slice', () => {
  const page = (events: EventDto[]) =>
    HttpResponse.json({ data: { events, next_cursor: null } })

  it('never raises `loading` once a slice is on screen', async () => {
    server.use(http.get(TIMELINE_PATH, () => page([event('$1', 100)])))
    const store = makeStore()
    await store.loadLatest()
    expect(store.events.value).toHaveLength(1)

    // Watch `loading` for the whole of the gap-fill, not just at its end.
    const seen: boolean[] = []
    const stop = store.loading.subscribe((value) => seen.push(value))

    server.use(
      http.get(TIMELINE_PATH, () => page([event('$2', 200), event('$1', 100)])),
    )
    await store.loadLatest()
    stop()

    expect(seen).not.toContain(true)
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$1', '$2'])
  })

  it('still raises `loading` for the very first page', async () => {
    server.use(http.get(TIMELINE_PATH, () => page([event('$1', 100)])))
    const store = makeStore()
    expect(store.loading.value).toBe(true)

    const seen: boolean[] = []
    const stop = store.loading.subscribe((value) => seen.push(value))
    await store.loadLatest()
    stop()

    expect(seen).toContain(true)
    expect(store.loading.value).toBe(false)
  })

  it('keeps the old slice visible when the refetch fails', async () => {
    server.use(http.get(TIMELINE_PATH, () => page([event('$1', 100)])))
    const store = makeStore()
    await store.loadLatest()

    server.use(
      http.get(TIMELINE_PATH, () => HttpResponse.json({}, { status: 503 })),
    )
    await store.loadLatest()

    expect(store.loading.value).toBe(false)
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$1'])
  })

  it('merges an overlapping head, keeping scroll-back and its cursor (WCR-05)', async () => {
    const cursors: (string | null)[] = []
    server.use(
      http.get(TIMELINE_PATH, ({ request }) => {
        const cursor = new URL(request.url).searchParams.get('cursor')
        cursors.push(cursor)
        if (cursor === 'c1') {
          // The scroll-back page below the head.
          return HttpResponse.json({
            data: {
              events: [event('$2', 200), event('$1', 100)],
              next_cursor: 'c0',
            },
          })
        }
        if (cursor === 'c0') {
          return HttpResponse.json({
            data: { events: [], next_cursor: null },
          })
        }
        // Head reads: first [$3], then (after "reconnect") [$4, $3-edited].
        return cursors.filter((c) => c === null).length === 1
          ? HttpResponse.json({
              data: { events: [event('$3', 300)], next_cursor: 'c1' },
            })
          : HttpResponse.json({
              data: {
                events: [
                  event('$4', 400),
                  event('$3', 300, { body: 'edited while offline' }),
                ],
                next_cursor: 'c2',
              },
            })
      }),
    )

    const store = makeStore()
    await store.loadLatest()
    await store.loadOlder() // slice: $1 $2 $3, cursor c0

    // Reconnect gap-fill: the fresh head overlaps ($3), so the loaded
    // history must survive, the head's row wins by id, and the *old* cursor
    // keeps paginating from where scroll-back left off.
    await store.loadLatest()
    expect(store.events.value.map((e) => e.event_id)).toEqual([
      '$1',
      '$2',
      '$3',
      '$4',
    ])
    expect(store.events.value[2].body).toBe('edited while offline')

    await store.loadOlder()
    expect(cursors.at(-1)).toBe('c0')
    expect(store.atStart.value).toBe(true)
  })

  it('replaces the slice when the fresh head no longer overlaps it (WCR-05)', async () => {
    let heads = 0
    server.use(
      http.get(TIMELINE_PATH, ({ request }) => {
        const cursor = new URL(request.url).searchParams.get('cursor')
        if (cursor !== null) {
          expect(cursor).toBe('c8')
          return HttpResponse.json({
            data: { events: [event('$8', 800)], next_cursor: null },
          })
        }
        heads += 1
        return heads === 1
          ? HttpResponse.json({
              data: { events: [event('$1', 100)], next_cursor: null },
            })
          : HttpResponse.json({
              data: { events: [event('$9', 900)], next_cursor: 'c8' },
            })
      }),
    )

    const store = makeStore()
    await store.loadLatest()
    await store.loadLatest() // no shared id: the gap is real — replace

    expect(store.events.value.map((e) => e.event_id)).toEqual(['$9'])
    // Scroll-back continues from the *new* head's cursor.
    await store.loadOlder()
    expect(store.events.value.map((e) => e.event_id)).toEqual(['$8', '$9'])
  })

  it('a merged head keeps pending echoes last and drops the one it confirms (WCR-05)', async () => {
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: { events: [event('$1', 100)], next_cursor: null },
        }),
      ),
      http.post(SEND_PATH, () =>
        HttpResponse.json({ data: { event_id: '$mine' } }),
      ),
      // Reconcile misses; the echo is still pending at gap-fill time.
      http.get(EVENT_PATH, () => new HttpResponse(null, { status: 404 })),
    )
    const store = makeStore()
    await store.loadLatest()
    await store.send('sent offline?', { senderId: '@alice:hs' })
    await store.send('still unconfirmed', { senderId: '@alice:hs' })

    // The reconnect head confirms the first send and brings a newcomer.
    server.use(
      http.get(TIMELINE_PATH, () =>
        HttpResponse.json({
          data: {
            events: [
              event('$mine', 300, {
                sender: '@alice:hs',
                body: 'sent offline?',
              }),
              event('$other', 250),
              event('$1', 100),
            ],
            next_cursor: null,
          },
        }),
      ),
    )
    await store.loadLatest()

    const ids = store.events.value.map((e) => e.event_id)
    expect(ids.slice(0, 3)).toEqual(['$1', '$other', '$mine'])
    // The unconfirmed echo survives, stays last, and stays pending.
    expect(ids).toHaveLength(4)
    expect(ids[3]).toMatch(/^local:/)
    expect(store.events.value[3].localEcho?.status).toBe('pending')
    expect(store.events.value[3].body).toBe('still unconfirmed')
  })
})
