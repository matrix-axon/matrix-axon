import { computed, signal } from '@preact/signals'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { createApiClient } from './api/client'
import { createAttachmentStaging } from './media/attachment-staging'
import { createMediaService } from './media/media-service'
import { TIMELINE_EVENT, UNREAD_COUNTS_CHANGED } from './api/frames'
import {
  connectCacheReset,
  connectCacheSetting,
  connectRoomsSessionReset,
  connectAttachmentReset,
  connectLiveRooms,
  connectLiveThreadUnread,
  connectReadMarkers,
  connectTimelineCacheReset,
  connectUnreadCounts,
  connectUpdateChecks,
} from './services'
import {
  createThreadUnreadStore,
  type ActiveThread,
} from './stores/thread-unread'
import type { AccountsStore } from './stores/accounts'
import { setPerfEnabled } from './perf'
import { createMemoryCacheStore } from './stores/cache-store'
import { createDeviceStateStore } from './stores/device-state'
import { createLiveConnection } from './stores/live-connection'
import { createTimelineStoreCache } from './stores/timeline-cache'
import { createUpdateChecker } from './stores/update-check'
import { FakeWebSocket } from './test/fake-socket'
import { memoryStorage } from './test/memory-storage'
import { testServices } from './test/services'

const ACCT = '11111111-1111-1111-1111-111111111111'
const ROOM = '!room:server'

function readMarkerHarness() {
  let socket: FakeWebSocket | undefined
  const live = createLiveConnection({
    socketFactory: () => {
      socket = new FakeWebSocket()
      return socket.asWebSocket()
    },
  })
  const api = createApiClient(
    {
      getToken: () => 't',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    },
    'http://axon.test',
  )
  const deviceState = createDeviceStateStore(api, live, memoryStorage())
  const { stub, unreadCounts } = roomsStub()
  connectReadMarkers(live, deviceState, stub)
  live.start()
  return { live, deviceState, unreadCounts, socket: () => socket! }
}

const readMarkerFrame = (deviceId: string, entries: Record<string, unknown>) =>
  JSON.stringify({
    type: 'device_state.changed',
    account_id: ACCT,
    payload: {
      device_id: deviceId,
      namespace: 'read_markers',
      entries,
      updated_at: '2026-01-01T00:00:00Z',
    },
  })

describe('connectReadMarkers', () => {
  it('clears unread when a sibling device reads a room', () => {
    const { unreadCounts, socket } = readMarkerHarness()
    socket().emitMessage(
      readMarkerFrame('sibling', { [ROOM]: { event_id: '$e', origin_ts: 1 } }),
    )
    expect(unreadCounts).toEqual([[ACCT, ROOM, 0, 0]])
  })

  it('ignores our own read-marker echo', () => {
    const { unreadCounts, deviceState, socket } = readMarkerHarness()
    socket().emitMessage(
      readMarkerFrame(deviceState.deviceId, {
        [ROOM]: { event_id: '$e', origin_ts: 1 },
      }),
    )
    expect(unreadCounts).toHaveLength(0)
  })

  it('ignores drafts frames', () => {
    const { unreadCounts, socket } = readMarkerHarness()
    socket().emitMessage(
      JSON.stringify({
        type: 'device_state.changed',
        account_id: ACCT,
        payload: {
          device_id: 'sibling',
          namespace: 'drafts',
          entries: { [ROOM]: { text: 'hi' } },
          updated_at: '2026-01-01T00:00:00Z',
        },
      }),
    )
    expect(unreadCounts).toHaveLength(0)
  })
})

function roomsStub() {
  const noted: [string, string, number][] = []
  const unreadCounts: [string, string, number, number][] = []
  const liveEvents: unknown[] = []
  let refreshes = 0
  const empty = signal<never[]>([])
  const stub = {
    rooms: computed(() => empty.value),
    unreadKeys: computed(() => new Set<string>()),
    unreadTotal: computed(() => 0),
    loading: computed(() => false),
    stale: computed(() => false),
    error: signal<string | null>(null),
    titles: computed(() => new Map<string, string>()),
    dmAvatars: computed(() => new Map<string, string>()),
    refresh: () => {
      refreshes += 1
      return Promise.resolve()
    },
    ensureLoaded: () => {
      refreshes += 1
      return Promise.resolve()
    },
    resetSession: () => {},
    leaveRoom: () => Promise.resolve({ ok: true as const }),
    forgetRoom: () => Promise.resolve({ ok: true as const }),
    joinRoom: () => Promise.resolve({ ok: true as const, roomId: ROOM }),
    knockRoom: () => Promise.resolve({ ok: true as const, roomId: ROOM }),
    createRoom: () => Promise.resolve({ ok: true as const, roomId: ROOM }),
    createDm: () => Promise.resolve({ ok: true as const, roomId: ROOM }),
    preview: () => undefined,
    unreadCount: () => 0,
    hydratePreview: () => {},
    noteActivity: (accountId: string, roomId: string, ts: number) => {
      noted.push([accountId, roomId, ts])
    },
    noteTimelineEvent: (event: unknown) => {
      liveEvents.push(event)
      if (
        typeof event === 'object' &&
        event !== null &&
        'account_id' in event &&
        'room_id' in event &&
        'origin_ts' in event
      ) {
        noted.push([
          (event as { account_id: string }).account_id,
          (event as { room_id: string }).room_id,
          (event as { origin_ts: number }).origin_ts,
        ])
      }
    },
    noteUnreadCounts: (
      accountId: string,
      roomId: string,
      notificationCount: number,
      highlightCount: number,
    ) => {
      unreadCounts.push([accountId, roomId, notificationCount, highlightCount])
    },
  }
  return {
    stub,
    noted,
    unreadCounts,
    liveEvents,
    refreshCount: () => refreshes,
  }
}

describe('connectUnreadCounts', () => {
  it('feeds server count frames into the rooms store', () => {
    let socket: FakeWebSocket | undefined
    const live = createLiveConnection({
      socketFactory: () => {
        socket = new FakeWebSocket()
        return socket.asWebSocket()
      },
    })
    const { stub, unreadCounts } = roomsStub()
    connectUnreadCounts(live, stub)
    live.start()

    socket!.emitMessage(
      JSON.stringify({
        type: UNREAD_COUNTS_CHANGED,
        account_id: ACCT,
        payload: {
          room_id: ROOM,
          notification_count: 3,
          highlight_count: 1,
        },
      }),
    )
    expect(unreadCounts).toEqual([[ACCT, ROOM, 3, 1]])

    socket!.emitMessage(
      JSON.stringify({
        type: UNREAD_COUNTS_CHANGED,
        account_id: ACCT,
        payload: { room_id: ROOM, notification_count: -1, highlight_count: 0 },
      }),
    )
    expect(unreadCounts).toHaveLength(1)
  })
})

describe('connectLiveRooms', () => {
  it('feeds timeline frames into noteActivity and ignores other kinds', () => {
    let socket: FakeWebSocket | undefined
    const live = createLiveConnection({
      socketFactory: () => {
        socket = new FakeWebSocket()
        return socket.asWebSocket()
      },
    })
    const { stub, noted, liveEvents } = roomsStub()
    connectLiveRooms(live, stub)
    live.start()

    socket!.emitMessage(
      JSON.stringify({
        type: TIMELINE_EVENT,
        account_id: ACCT,
        payload: {
          event_id: '$e',
          account_id: ACCT,
          room_id: ROOM,
          origin_ts: 123,
          arrival_order: 123,
        },
      }),
    )
    expect(noted).toEqual([[ACCT, ROOM, 123]])
    expect(liveEvents).toHaveLength(1)

    socket!.emitMessage(
      JSON.stringify({
        type: 'device_state.changed',
        account_id: ACCT,
        payload: {},
      }),
    )
    expect(noted).toHaveLength(1)
  })

  it('re-reads the list on reconnect, not on the first connect (WCR-08)', () => {
    vi.useFakeTimers()
    const sockets: FakeWebSocket[] = []
    const live = createLiveConnection({
      socketFactory: () => {
        const s = new FakeWebSocket()
        sockets.push(s)
        return s.asWebSocket()
      },
    })
    const { stub, refreshCount } = roomsStub()
    connectLiveRooms(live, stub)
    live.start()

    sockets[0].emitOpen()
    expect(refreshCount()).toBe(0) // first connect is not a reconnect

    sockets[0].emitClose()
    vi.advanceTimersByTime(1000) // initial backoff
    sockets[1].emitOpen()
    expect(refreshCount()).toBe(1)
    vi.useRealTimers()
  })
})

function threadUnreadHarness(now: () => number) {
  let socket: FakeWebSocket | undefined
  const live = createLiveConnection({
    socketFactory: () => {
      socket = new FakeWebSocket()
      return socket.asWebSocket()
    },
  })
  const threadUnread = createThreadUnreadStore()
  const accounts = {
    accounts: computed(() => [{ account_id: ACCT, user_id: '@me:server' }]),
  } as unknown as AccountsStore
  const { stub } = roomsStub()
  const activeThread = signal<ActiveThread | null>(null)
  connectLiveThreadUnread(live, stub, accounts, threadUnread, activeThread, now)
  live.start()
  socket!.emitOpen()
  return { threadUnread, socket: () => socket! }
}

const threadReplyFrame = (originTs: number) =>
  JSON.stringify({
    type: TIMELINE_EVENT,
    account_id: ACCT,
    payload: {
      event_id: `$reply-${originTs}`,
      account_id: ACCT,
      room_id: ROOM,
      sender: '@alice:server',
      origin_ts: originTs,
      arrival_order: originTs,
      relates_to: { rel_type: 'm.thread', event_id: '$root' },
      body: 'a reply',
    },
  })

describe('connectLiveThreadUnread', () => {
  // A homeserver-stamped `origin_ts`, so the harness clock is fixed near it.
  const T0 = 1_700_000_000_000

  it('badges a thread for a reply stamped after the connection opened', () => {
    const { threadUnread, socket } = threadUnreadHarness(() => T0)
    socket().emitMessage(threadReplyFrame(T0 + 1000))
    expect(threadUnread.count.value).toBe(1)
  })

  it('keeps a reply inside the clock-skew slack window', () => {
    const { threadUnread, socket } = threadUnreadHarness(() => T0)
    socket().emitMessage(threadReplyFrame(T0 - 60_000))
    expect(threadUnread.count.value).toBe(1)
  })

  it('drops a dormant-room reply replayed on a gappy resync', () => {
    const { threadUnread, socket } = threadUnreadHarness(() => T0)
    socket().emitMessage(threadReplyFrame(T0 - 365 * 24 * 60 * 60_000))
    expect(threadUnread.count.value).toBe(0)
  })
})

describe('connectUpdateChecks', () => {
  // A deploy restarts the process the socket runs through, so a reconnect is
  // the earliest evidence that the build may have changed — seconds, without
  // polling. The first connect is not evidence of anything.
  it('checks for a new build on reconnect, not on the first connect', () => {
    vi.useFakeTimers()
    try {
      const sockets: FakeWebSocket[] = []
      const live = createLiveConnection({
        socketFactory: () => {
          const s = new FakeWebSocket()
          sockets.push(s)
          return s.asWebSocket()
        },
      })
      let checks = 0
      const updates = createUpdateChecker({
        currentVersion: 'build-a',
        fetchManifest: () => {
          checks += 1
          return Promise.resolve(null)
        },
      })
      connectUpdateChecks(live, updates)
      live.start()

      sockets[0].emitOpen()
      expect(checks).toBe(0)

      sockets[0].emitClose()
      vi.advanceTimersByTime(1000) // initial backoff
      sockets[1].emitOpen()
      expect(checks).toBe(1)
    } finally {
      vi.useRealTimers()
    }
  })
})

describe('connectTimelineCacheReset', () => {
  /** Only the two members the connector touches; the rest of the graph is irrelevant. */
  function harness(signedIn: boolean) {
    const flag = signal(signedIn)
    let clears = 0
    const dispose = connectTimelineCacheReset(
      { signedIn: computed(() => flag.value) } as Parameters<
        typeof connectTimelineCacheReset
      >[0],
      {
        acquire: () => {
          throw new Error('unused')
        },
        clear: () => {
          clears += 1
        },
        hasUnsentWork: false,
        size: 0,
      },
    )
    return { flag, clears: () => clears, dispose }
  }

  it('wipes the warm stores when the session ends', () => {
    const { flag, clears, dispose } = harness(true)
    expect(clears()).toBe(0)

    flag.value = false

    // The service graph outlives a sign-out, so without this a signed-out tab
    // keeps every warm room's messages in memory (ADR 0085 phase 1).
    expect(clears()).toBe(1)
    dispose()
  })

  it('starts from a clean cache when the app boots signed out', () => {
    const { clears, dispose } = harness(false)

    expect(clears()).toBe(1)
    dispose()
  })

  it('settles the flush when signing out with an unsent send staged', async () => {
    // The wipe runs *inside* the reactive flush, and disposing a store that
    // holds a local echo writes a signal. That is only safe while the wipe is
    // idempotent: `clear()` empties the map, so the effect's next pass in the
    // same flush writes nothing. Make it write every pass and @preact/signals
    // gives up after 100 iterations with "Cycle detected" — the flush never
    // reaches a fixed point. Nothing else in this suite has an echo staged at
    // sign-out, which is the only case where the wipe writes at all.
    const auth = {
      getToken: () => 'tok-test',
      onAuthFailure: () => {},
      LoginBootstrap: () => null,
    }
    const api = createApiClient(auth, 'http://axon.test')
    const timelines = createTimelineStoreCache(
      api,
      createMediaService({ auth, baseUrl: 'http://axon.test' }),
    )
    const store = timelines.acquire(ACCT, ROOM)
    const send = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response('', { status: 502 }))
    expect(await store.send('unsent draft')).toBe(false)
    expect(store.events.peek()).toHaveLength(1)
    send.mockRestore()

    const flag = signal(true)
    const dispose = connectTimelineCacheReset(
      { signedIn: computed(() => flag.value) } as Parameters<
        typeof connectTimelineCacheReset
      >[0],
      timelines,
    )

    expect(() => {
      flag.value = false
    }).not.toThrow()
    expect(timelines.size).toBe(0)
    dispose()
  })
})

describe('connectCacheReset', () => {
  function harness(signedIn: boolean) {
    const flag = signal(signedIn)
    const cache = createMemoryCacheStore()
    const dispose = connectCacheReset(
      { signedIn: computed(() => flag.value) } as Parameters<
        typeof connectCacheReset
      >[0],
      cache,
    )
    return { flag, cache, dispose }
  }

  it('wipes every area on sign-out, not only the account that left', async () => {
    const { flag, cache, dispose } = harness(true)
    await cache.write('rooms', 'reader-one', ['a'])
    await cache.write('rooms', 'reader-two', ['b'])
    await cache.write('timelines', 'reader-one', ['c'])

    flag.value = false
    await Promise.resolve()

    expect(await cache.keys('rooms')).toEqual([])
    expect(await cache.keys('timelines')).toEqual([])
    dispose()
  })

  it('drops a cache left behind by an evicted session on a signed-out boot', async () => {
    const cache = createMemoryCacheStore()
    await cache.write('rooms', 'leftover', ['a'])
    const dispose = connectCacheReset(
      { signedIn: computed(() => false) } as Parameters<
        typeof connectCacheReset
      >[0],
      cache,
    )
    await Promise.resolve()

    expect(await cache.keys('rooms')).toEqual([])
    dispose()
  })
})

describe('connectCacheSetting', () => {
  it('erases what is on disk when the setting is turned off', async () => {
    const cacheRoomList = signal(true)
    const cache = createMemoryCacheStore()
    await cache.write('rooms', 'reader', ['a'])
    const dispose = connectCacheSetting(
      { cacheRoomList } as Parameters<typeof connectCacheSetting>[0],
      cache,
    )
    expect(await cache.keys('rooms')).toEqual(['reader'])

    cacheRoomList.value = false
    await Promise.resolve()

    expect(await cache.keys('rooms')).toEqual([])
    dispose()
  })
})

describe('instrumentation at graph construction (ADR 0085 boot summary)', () => {
  afterEach(() => {
    setPerfEnabled(false)
    performance.clearMarks()
  })

  it('records the cache-read marks when perf is on from Settings', () => {
    // The failure this pins is invisible: `App` applies the stored preference
    // in an effect, which runs long after `createServices` has built the rooms
    // store and marked its cache read, so those marks were dropped and the
    // boot summary reported `read=null`. `?perf=1` masked it entirely — it
    // latches inside `perfEnabled()` before any store exists, which is exactly
    // the configuration the e2e lane uses.
    performance.clearMarks()
    testServices({
      storage: memoryStorage({
        'axon.token': 'tok-test',
        'axon.settings': JSON.stringify({ version: 1, perfMarks: true }),
      }),
    })

    const names = performance.getEntriesByType('mark').map((mark) => mark.name)
    expect(names).toContain('axon:rooms:cache:read:start')
  })

  it('stays silent when the preference is off', () => {
    performance.clearMarks()
    testServices({
      storage: memoryStorage({
        'axon.token': 'tok-test',
        'axon.settings': JSON.stringify({ version: 1, perfMarks: false }),
      }),
    })

    const names = performance.getEntriesByType('mark').map((mark) => mark.name)
    expect(names).not.toContain('axon:rooms:cache:read:start')
  })
})

describe('connectRoomsSessionReset', () => {
  function harness(signedIn: boolean) {
    const flag = signal(signedIn)
    let resets = 0
    const { stub } = roomsStub()
    const dispose = connectRoomsSessionReset(
      { signedIn: computed(() => flag.value) } as Parameters<
        typeof connectRoomsSessionReset
      >[0],
      { ...stub, resetSession: () => (resets += 1) },
    )
    return { flag, resets: () => resets, dispose }
  }

  it('abandons the room list when the session ends', () => {
    const { flag, resets, dispose } = harness(true)
    expect(resets()).toBe(0)

    flag.value = false

    // Without this the next reader inherits the previous one's rows, and a
    // request still in flight under the old token is applied — and cached —
    // under the new reader's identity.
    expect(resets()).toBe(1)
    dispose()
  })

  it('does not throw when it runs against an already-clean store', () => {
    // Driven from an effect, so it must be idempotent: an unconditional signal
    // write here would not settle the flush ("Cycle detected").
    const flag = signal(true)
    const services = testServices()
    const dispose = connectRoomsSessionReset(
      { signedIn: computed(() => flag.value) } as Parameters<
        typeof connectRoomsSessionReset
      >[0],
      services.rooms,
    )

    expect(() => {
      flag.value = false
    }).not.toThrow()
    expect(services.rooms.rooms.value).toEqual([])
    dispose()
  })

  it('clears the persisted room-title cache on sign-out', () => {
    // The whole user-visible path: Sign out -> `clearToken` -> `signedIn` ->
    // this effect -> `resetSession`. For a DM the cached title is the other
    // person's display name, and it must not survive the session (#115).
    const storage = memoryStorage({
      'axon.token': 'tok-test',
      'axon.room_titles.v1': JSON.stringify({
        version: 1,
        titles: [['acct/!dm:hs', 'Bob']],
      }),
    })
    const services = testServices({ storage })
    expect(services.auth.signedIn.value).toBe(true)
    expect(storage.getItem('axon.room_titles.v1')).not.toBeNull()

    services.auth.clearToken()

    expect(services.auth.signedIn.value).toBe(false)
    expect(storage.getItem('axon.room_titles.v1')).toBeNull()
    // Sign-out is targeted: unrelated device-local state survives.
    expect(storage.getItem('axon.device_id')).not.toBeNull()
  })
})

describe('connectAttachmentReset', () => {
  it('drops staged files when the session ends', async () => {
    // The user's own files, held in memory with live object urls, in a graph
    // that outlives a sign-out (issue #89). Before retention they died with the
    // composer's unmount and needed nothing said.
    const flag = signal(true)
    let cleared = 0
    const dispose = connectAttachmentReset(
      { signedIn: computed(() => flag.value) } as Parameters<
        typeof connectAttachmentReset
      >[0],
      {
        clearAll: () => {
          cleared += 1
        },
      } as unknown as Parameters<typeof connectAttachmentReset>[1],
    )

    flag.value = false

    expect(cleared).toBe(1)
    dispose()
  })

  it('settles the flush when signing out with files staged', () => {
    // The wipe runs inside the reactive flush, so it must be idempotent:
    // `clearAll()` bumps the staging revision, and a bump that fires even when
    // there was nothing to drop re-triggers this effect, which wipes and bumps
    // again. That flush never reaches a fixed point and @preact/signals aborts
    // it after 100 iterations with "Cycle detected" — which is what an
    // unconditional bump here actually did, in every test that renders the
    // signed-in shell. Staging real files is what makes the wipe write at all.
    const staging = createAttachmentStaging()
    staging.stage('scope', [new File(['x'], 'a.txt', { type: 'text/plain' })])
    expect(staging.batch('scope').items).toHaveLength(1)

    const flag = signal(true)
    const dispose = connectAttachmentReset(
      { signedIn: computed(() => flag.value) } as Parameters<
        typeof connectAttachmentReset
      >[0],
      staging,
    )

    expect(() => {
      flag.value = false
    }).not.toThrow()
    expect(staging.batch('scope').items).toHaveLength(0)
    dispose()
  })
})
