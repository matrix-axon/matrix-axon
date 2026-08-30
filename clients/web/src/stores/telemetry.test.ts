import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createMemoryCacheStore, type CacheStore } from './cache-store'
import {
  createTelemetryStore,
  formatTelemetry,
  scrub,
  type TelemetryStore,
} from './telemetry'

/** Let the coalesced write land. */
const flushed = () => new Promise((resolve) => setTimeout(resolve, 0))

describe('telemetry store', () => {
  let cache: CacheStore
  let store: TelemetryStore
  let enabled: boolean

  beforeEach(() => {
    cache = createMemoryCacheStore()
    enabled = true
    store = createTelemetryStore({
      cache,
      enabled: () => enabled,
      sessionId: () => 'session-one',
      now: () => 1_700_000_000_000,
    })
  })
  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('keeps the summary marks and nothing else', async () => {
    store.record('boot:room-open', 1200, { rows: 1160, net: 1158 })
    // The raw stream carries identifiers; only the reduced summaries may be
    // written. A name allow-list is the mechanism, not a promise about intent.
    store.record('room-page:initial-load-effect', 30, {
      accountId: 'a',
      roomId: '!secret:hs',
    })
    store.record('room-row:hydrate-preview', 40, { key: '!secret:hs' })
    await flushed()

    const [session] = await store.read()
    expect(session.entries.map((entry) => entry.name)).toEqual([
      'boot:room-open',
    ])
  })

  it('drops an identifier-shaped value even from an allowed mark', async () => {
    // The allow-list stops a *mark* leaking; it cannot stop a field being
    // added to one of the four later. Matrix ids and UUIDs have distinct
    // shapes, so they are refused on sight.
    store.record('boot:room-open', 100, {
      rows: 5,
      roomId: '!added:later.example',
      accountId: '2b1e5f0a-1c3d-4e5f-8a9b-0c1d2e3f4a5b',
      user: '@someone:hs',
      phase: 'settled',
    })
    await flushed()

    const [session] = await store.read()
    expect(session.entries[0].detail).toEqual({ rows: 5, phase: 'settled' })
  })

  it('keeps the collapsed route shape, which names nothing', async () => {
    // `shortRoute` has already replaced the ids, so this must survive — an
    // over-strict scrub would silently empty the most useful line.
    store.record('boot:room-open:req', 100, {
      route: 'accounts/{account}/rooms/{id}/timeline',
      total: 1340,
      gzip: true,
      proto: 'h2',
    })
    await flushed()

    const [session] = await store.read()
    expect(session.entries[0].detail.route).toBe(
      'accounts/{account}/rooms/{id}/timeline',
    )
    expect(session.entries[0].detail.proto).toBe('h2')
  })

  it('writes nothing at all while the setting is off', async () => {
    enabled = false
    store.record('boot:room-open', 100, { rows: 5 })
    await flushed()

    expect(await store.read()).toEqual([])
  })

  it('survives a session that never stops recording', async () => {
    for (let index = 0; index < 260; index += 1) {
      store.record('boot:room-open', index, { rows: index })
    }
    await flushed()

    const [session] = await store.read()
    expect(session.entries).toHaveLength(200)
    // The newest are what a reader wants; the oldest are what gets dropped.
    expect(session.entries[199].detail.rows).toBe(259)
  })

  it('loses to a wipe that lands while it is writing', async () => {
    // Same rule the room-list cache applies: a privacy barrier must beat a
    // write already on its way, not lose to it.
    store.record('boot:room-open', 100, { rows: 5 })
    void cache.clear()
    await flushed()

    expect(await store.read()).toEqual([])
  })

  it('keeps other sessions when a new one writes', async () => {
    store.record('boot:room-open', 100, { rows: 5 })
    await flushed()

    const second = createTelemetryStore({
      cache,
      enabled: () => true,
      sessionId: () => 'session-two',
      now: () => 1_700_000_001_000,
    })
    second.record('boot:room-list', 200, { saved: 519 })
    await flushed()

    const sessions = await second.read()
    expect(sessions.map((each) => each.id)).toEqual([
      'session-one',
      'session-two',
    ])
  })
})

describe('scrub', () => {
  it('refuses anything that is not a vetted scalar', () => {
    expect(
      scrub({
        n: 1,
        b: true,
        nul: null,
        ok: 'settled',
        room: '!a:b',
        user: '@a:b',
        alias: '#a:b',
        event: '$abc',
        nested: { a: 1 },
        list: [1, 2],
      }),
    ).toEqual({ n: 1, b: true, nul: null, ok: 'settled' })
  })

  it('tolerates a mark with no detail', () => {
    expect(scrub(undefined)).toEqual({})
    expect(scrub(null)).toEqual({})
  })
})

describe('formatTelemetry', () => {
  it('renders the same shape the overlay shows', () => {
    const text = formatTelemetry([
      {
        id: 'abc',
        startedAt: 1_700_000_000_000,
        entries: [
          {
            at: 1230,
            name: 'boot:room-list',
            detail: { saved: 519, rows: 688 },
          },
        ],
      },
    ])
    expect(text).toContain('# session abc')
    expect(text).toContain('1230 boot:room-list saved=519 rows=688')
  })

  it('says so plainly when there is nothing recorded', () => {
    expect(formatTelemetry([])).toBe('No telemetry recorded.')
  })
})
