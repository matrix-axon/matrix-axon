import { computed, signal } from '@preact/signals'
import { describe, expect, it, vi } from 'vitest'
import { createApiClient } from './api/client'
import { createMediaService } from './media/media-service'
import { TIMELINE_EVENT, UNREAD_COUNTS_CHANGED } from './api/frames'
import {
  connectLiveRooms,
  connectReadMarkers,
  connectTimelineCacheReset,
  connectUnreadCounts,
} from './services'
import { createDeviceStateStore } from './stores/device-state'
import { createLiveConnection } from './stores/live-connection'
import { createTimelineStoreCache } from './stores/timeline-cache'
import { FakeWebSocket } from './test/fake-socket'
import { memoryStorage } from './test/memory-storage'

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
    error: signal<string | null>(null),
    titles: computed(() => new Map<string, string>()),
    refresh: () => {
      refreshes += 1
      return Promise.resolve()
    },
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
