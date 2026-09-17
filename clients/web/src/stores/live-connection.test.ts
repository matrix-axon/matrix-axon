import { describe, expect, it, vi } from 'vitest'
import { HEARTBEAT, TIMELINE_EVENT, type LiveFrame } from '../api/frames'
import { setPerfEnabled } from '../perf'
import { FakeWebSocket } from '../test/fake-socket'
import {
  createLiveConnection,
  HEARTBEAT_TIMEOUT_MS,
  REVIVE_AFTER_HIDDEN_MS,
  REVIVE_COALESCE_MS,
  STABLE_CONNECTION_MS,
} from './live-connection'

/**
 * Build a connection over a captured fake socket. `socket` is set when
 * `start()` runs the factory, so read it only after starting.
 */
function harness() {
  let socket: FakeWebSocket | undefined
  let created = 0
  const live = createLiveConnection({
    socketFactory: () => {
      created += 1
      socket = new FakeWebSocket()
      return socket.asWebSocket()
    },
  })
  return { live, socket: () => socket!, created: () => created }
}

const frame = (type: string, accountId = 'acct-1', payload: unknown = {}) =>
  JSON.stringify({ type, account_id: accountId, payload })

/** A hand-driven stand-in for `window`/`document`'s listener registry. */
function fakeTarget() {
  const handlers = new Map<string, Set<EventListener>>()
  return {
    hidden: false,
    addEventListener(type: string, listener: EventListener) {
      const forType = handlers.get(type) ?? new Set<EventListener>()
      forType.add(listener)
      handlers.set(type, forType)
    },
    removeEventListener(type: string, listener: EventListener) {
      handlers.get(type)?.delete(listener)
    },
    /** Fire every listener registered for `type`. */
    emit(type: string) {
      for (const listener of [...(handlers.get(type) ?? [])]) {
        listener(new Event(type))
      }
    },
    /** How many listeners are registered for `type` — teardown's assertion. */
    listeners(type: string) {
      return handlers.get(type)?.size ?? 0
    },
  }
}

/**
 * `harness()` over injected event targets and a movable clock, for the
 * network-change paths. Each `socketFactory` call is recorded, so a test can
 * assert that the socket was *replaced* rather than merely poked.
 */
function networkHarness() {
  const sockets: FakeWebSocket[] = []
  const win = fakeTarget()
  const doc = fakeTarget()
  let nowMs = 1_000
  const live = createLiveConnection({
    socketFactory: () => {
      const socket = new FakeWebSocket()
      sockets.push(socket)
      return socket.asWebSocket()
    },
    window: win,
    document: doc,
    now: () => nowMs,
  })
  return {
    live,
    win,
    doc,
    sockets,
    latest: () => sockets[sockets.length - 1]!,
    advanceClock: (ms: number) => {
      nowMs += ms
    },
  }
}

describe('createLiveConnection', () => {
  it('starts offline, goes connecting on start, then live on open', () => {
    const { live, socket } = harness()
    expect(live.connection.value).toBe('offline')
    live.start()
    expect(live.connection.value).toBe('connecting')
    socket().emitOpen()
    expect(live.connection.value).toBe('live')
  })

  // The room-open readout counts reconnects inside an open, since one landing
  // mid-open is what doubled its head loads (#389).
  it('marks each socket open and drop for the room-open readout', () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    performance.clearMarks()
    setPerfEnabled(true)
    try {
      const { live, socket } = harness()
      live.start()
      socket().emitOpen()
      socket().emitClose()
      vi.runOnlyPendingTimers()
      socket().emitOpen()

      const marks = performance
        .getEntriesByType('mark')
        .filter((mark) => mark.name.startsWith('axon:live:'))
        .map((mark) => [mark.name, (mark as PerformanceMark).detail])
      expect(marks).toEqual([
        ['axon:live:open', { reconnect: false }],
        ['axon:live:close', null],
        ['axon:live:open', { reconnect: true }],
      ])
    } finally {
      setPerfEnabled(false)
      performance.clearMarks()
      vi.useRealTimers()
    }
  })

  it('delivers decoded frames to every subscriber', () => {
    const { live, socket } = harness()
    const a = vi.fn()
    const b = vi.fn()
    live.subscribe(a)
    live.subscribe(b)
    live.start()
    socket().emitOpen()
    socket().emitMessage(frame(TIMELINE_EVENT))

    const expected: LiveFrame = {
      type: TIMELINE_EVENT,
      accountId: 'acct-1',
      payload: {},
    }
    expect(a).toHaveBeenCalledWith(expected)
    expect(b).toHaveBeenCalledWith(expected)
  })

  it('delivers unknown tags too — the router is generic, consumers filter', () => {
    const { live, socket } = harness()
    const listener = vi.fn()
    live.subscribe(listener)
    live.start()
    socket().emitMessage(frame('sender_trust.violation'))
    expect(listener).toHaveBeenCalledWith(
      expect.objectContaining({ type: 'sender_trust.violation' }),
    )
  })

  it('drops malformed frames without notifying listeners', () => {
    const { live, socket } = harness()
    const listener = vi.fn()
    live.subscribe(listener)
    live.start()
    socket().emitMessage('not json')
    socket().emitMessage(42)
    expect(listener).not.toHaveBeenCalled()
  })

  it('one throwing listener does not starve the others', () => {
    const { live, socket } = harness()
    const good = vi.fn()
    live.subscribe(() => {
      throw new Error('boom')
    })
    live.subscribe(good)
    live.start()
    socket().emitMessage(frame(TIMELINE_EVENT))
    expect(good).toHaveBeenCalledOnce()
  })

  it('stops delivering after unsubscribe', () => {
    const { live, socket } = harness()
    const listener = vi.fn()
    const off = live.subscribe(listener)
    live.start()
    off()
    socket().emitMessage(frame(TIMELINE_EVENT))
    expect(listener).not.toHaveBeenCalled()
  })

  it('is idempotent: a second start does not open a second socket', () => {
    let count = 0
    const live = createLiveConnection({
      socketFactory: () => {
        count += 1
        return new FakeWebSocket().asWebSocket()
      },
    })
    live.start()
    live.start()
    expect(count).toBe(1)
  })

  it('goes offline and closes the socket on stop', () => {
    const { live, socket } = harness()
    live.start()
    socket().emitOpen()
    const opened = socket()
    live.stop()
    expect(live.connection.value).toBe('offline')
    expect(opened.closed).toBe(true)
  })

  it('goes reconnecting on an unexpected close', () => {
    const { live, socket } = harness()
    live.start()
    socket().emitOpen()
    socket().emitClose()
    expect(live.connection.value).toBe('reconnecting')
  })

  it('reconnects after a drop with backoff, bumping reconnects on reopen', () => {
    vi.useFakeTimers()
    const { live, socket, created } = harness()
    live.start()
    socket().emitOpen()
    expect(live.reconnects.value).toBe(0)

    socket().emitClose()
    expect(live.connection.value).toBe('reconnecting')
    vi.advanceTimersByTime(999)
    expect(created()).toBe(1) // still within the 1s backoff
    vi.advanceTimersByTime(1)
    expect(created()).toBe(2) // reconnect attempt fired

    socket().emitOpen()
    expect(live.connection.value).toBe('live')
    expect(live.reconnects.value).toBe(1)
    vi.useRealTimers()
  })

  it('doubles the backoff on repeated failures and caps it at 30s', () => {
    vi.useFakeTimers()
    const { live, socket, created } = harness()
    live.start()
    socket().emitOpen()

    // Each attempt closes before opening; the wait grows 1→2→4→…→30s (capped).
    const delays = [1000, 2000, 4000, 8000, 16000, 30000, 30000]
    let expected = 1
    for (const delay of delays) {
      socket().emitClose()
      vi.advanceTimersByTime(delay - 1)
      expect(created()).toBe(expected)
      vi.advanceTimersByTime(1)
      expected += 1
      expect(created()).toBe(expected)
    }
    vi.useRealTimers()
  })

  it('resets the backoff only after a connection stays up', () => {
    vi.useFakeTimers()
    const { live, socket, created } = harness()
    live.start()
    socket().emitOpen()

    socket().emitClose()
    vi.advanceTimersByTime(1000)
    expect(created()).toBe(2)

    // Accept-then-immediately-close (e.g. auth revoked post-handshake) is
    // not recovery: the backoff keeps growing instead of hammering at 1s.
    socket().emitOpen()
    socket().emitClose()
    vi.advanceTimersByTime(1999)
    expect(created()).toBe(2)
    vi.advanceTimersByTime(1) // this wait was 2s, not a reset 1s
    expect(created()).toBe(3)

    // A connection that stays up past the stability window is recovery:
    // the next drop starts over at 1s.
    socket().emitOpen()
    vi.advanceTimersByTime(STABLE_CONNECTION_MS)
    socket().emitClose()
    vi.advanceTimersByTime(999)
    expect(created()).toBe(3)
    vi.advanceTimersByTime(1)
    expect(created()).toBe(4)
    vi.useRealTimers()
  })

  it('cancels a pending reconnect on stop', () => {
    vi.useFakeTimers()
    const { live, socket, created } = harness()
    live.start()
    socket().emitOpen()
    socket().emitClose() // schedules a reconnect
    live.stop()
    expect(live.connection.value).toBe('offline')
    vi.advanceTimersByTime(60000)
    expect(created()).toBe(1) // no reconnect fired
    vi.useRealTimers()
  })

  it('keeps retrying when the socket factory fails (e.g. no token yet)', () => {
    vi.useFakeTimers()
    let attempts = 0
    let socket: FakeWebSocket | undefined
    const live = createLiveConnection({
      socketFactory: () => {
        attempts += 1
        if (attempts < 3) {
          throw new Error('no token')
        }
        socket = new FakeWebSocket()
        return socket.asWebSocket()
      },
    })
    live.start()
    expect(live.connection.value).toBe('reconnecting')
    vi.advanceTimersByTime(1000)
    vi.advanceTimersByTime(2000)
    expect(attempts).toBe(3)
    socket!.emitOpen()
    expect(live.connection.value).toBe('live')
    live.stop()
    vi.useRealTimers()
  })

  it('can restart after stop, and listeners survive the restart', () => {
    const { live, socket } = harness()
    const listener = vi.fn()
    live.subscribe(listener)
    live.start()
    live.stop()
    live.start()
    socket().emitMessage(frame(TIMELINE_EVENT))
    expect(listener).toHaveBeenCalledOnce()
  })

  it('ignores a stale socket close after stop', () => {
    const { live, socket } = harness()
    live.start()
    const first = socket()
    live.stop()
    live.start()
    socket().emitOpen()
    // The first socket closing late must not knock the live one offline.
    first.emitClose()
    expect(live.connection.value).toBe('live')
  })
})

describe('a socket factory that throws', () => {
  it('reports the reason once, not on every retry', () => {
    // The case this exists for: `api/ws.ts` throws a description of a base URL
    // that cannot produce a websocket URL. Before, every such throw was
    // swallowed and the client reconnected forever with nothing anywhere
    // saying why — the exact state the diagnostic was written to end.
    vi.useFakeTimers()
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    const live = createLiveConnection({
      socketFactory: () => {
        throw new Error('cannot derive a websocket URL from tauri:')
      },
    })
    live.start()
    vi.advanceTimersByTime(60_000)

    const ours = warn.mock.calls.filter((c) =>
      String(c[1]).includes('cannot derive a websocket URL'),
    )
    expect(ours).toHaveLength(1)
    expect(live.connection.value).toBe('reconnecting')

    live.stop()
    warn.mockRestore()
  })

  it('reports again when the reason changes', () => {
    vi.useFakeTimers()
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    let reason = 'no token yet'
    const live = createLiveConnection({
      socketFactory: () => {
        throw new Error(reason)
      },
    })
    live.start()
    vi.advanceTimersByTime(5_000)
    reason = 'cannot derive a websocket URL from tauri:'
    vi.advanceTimersByTime(60_000)

    const messages = warn.mock.calls.map((c) => String(c[1]))
    expect(messages).toContain('no token yet')
    expect(messages).toContain('cannot derive a websocket URL from tauri:')

    live.stop()
    warn.mockRestore()
  })

  /**
   * The iPhone WiFi→cell report. The handover leaves the old socket `OPEN` but
   * unroutable: no `close` ever fires, so the drop path never runs, the state
   * still reads `live`, and every consumer waits on frames that cannot arrive.
   * Nothing in the transport can report this — the repair has to come from the
   * network events the page gets instead.
   */
  describe('network changes', () => {
    it('replaces the socket on `online` and bumps reconnects', () => {
      const h = networkHarness()
      h.live.start()
      h.latest().emitOpen()
      expect(h.live.connection.value).toBe('live')
      expect(h.live.reconnects.value).toBe(0)

      // The old socket is still OPEN and still useless.
      const stale = h.latest()
      h.win.emit('online')

      expect(stale.closed).toBe(true)
      expect(h.sockets).toHaveLength(2)
      expect(h.live.connection.value).toBe('connecting')

      h.latest().emitOpen()
      // The gap-fill every consumer keys off (ADR 0061) is the actual repair.
      expect(h.live.reconnects.value).toBe(1)
      expect(h.live.connection.value).toBe('live')

      h.live.stop()
    })

    it('replaces the socket when a long-hidden page comes back', () => {
      const h = networkHarness()
      h.live.start()
      h.latest().emitOpen()

      h.doc.hidden = true
      h.doc.emit('visibilitychange')
      h.advanceClock(REVIVE_AFTER_HIDDEN_MS)
      h.doc.hidden = false
      h.doc.emit('visibilitychange')

      expect(h.sockets).toHaveLength(2)
      h.latest().emitOpen()
      expect(h.live.reconnects.value).toBe(1)

      h.live.stop()
    })

    /**
     * Replacing the socket costs every consumer a refetch, so a flick to
     * another tab must not trigger one — only an absence long enough for the
     * phone to have changed networks.
     */
    it('leaves the socket alone after a brief hide', () => {
      const h = networkHarness()
      h.live.start()
      h.latest().emitOpen()

      h.doc.hidden = true
      h.doc.emit('visibilitychange')
      h.advanceClock(REVIVE_AFTER_HIDDEN_MS - 1)
      h.doc.hidden = false
      h.doc.emit('visibilitychange')

      expect(h.sockets).toHaveLength(1)
      expect(h.live.reconnects.value).toBe(0)
      expect(h.live.connection.value).toBe('live')

      h.live.stop()
    })

    /**
     * A phone at the edge of coverage can fire `online` several times in a few
     * seconds. Replacing the socket for each one tears down connections that
     * just opened and bills every consumer a gap-fill refetch per cycle,
     * precisely when the network can least afford it.
     */
    it('coalesces a burst of triggers into one replacement', () => {
      const h = networkHarness()
      h.live.start()
      h.latest().emitOpen()

      h.win.emit('online')
      expect(h.sockets).toHaveLength(2)
      h.latest().emitOpen()

      // Three more within the window: all dropped.
      h.advanceClock(REVIVE_COALESCE_MS - 1)
      h.win.emit('online')
      h.win.emit('online')
      h.win.emit('online')
      expect(h.sockets).toHaveLength(2)
      expect(h.live.reconnects.value).toBe(1)

      // Past the window, a genuine later change still acts.
      h.advanceClock(1)
      h.win.emit('online')
      expect(h.sockets).toHaveLength(3)

      h.live.stop()
    })

    it('does not carry the window across a restart', () => {
      const h = networkHarness()
      h.live.start()
      h.latest().emitOpen()
      h.win.emit('online')
      expect(h.sockets).toHaveLength(2)
      h.live.stop()

      // No clock movement: a new session must not inherit the old window.
      h.live.start()
      h.latest().emitOpen()
      h.win.emit('online')

      expect(h.sockets).toHaveLength(4)

      h.live.stop()
    })

    it('ignores network events while no connection is wanted', () => {
      const h = networkHarness()
      h.live.start()
      h.latest().emitOpen()
      h.live.stop()

      h.win.emit('online')

      expect(h.sockets).toHaveLength(1)
      expect(h.live.connection.value).toBe('offline')
    })

    it('unregisters its listeners on stop', () => {
      const h = networkHarness()
      h.live.start()
      expect(h.win.listeners('online')).toBe(1)
      expect(h.doc.listeners('visibilitychange')).toBe(1)

      h.live.stop()

      expect(h.win.listeners('online')).toBe(0)
      expect(h.doc.listeners('visibilitychange')).toBe(0)
    })

    it('resets the backoff, so a revive reconnects without waiting', () => {
      vi.useFakeTimers()
      try {
        const h = networkHarness()
        h.live.start()
        // Grow the backoff the ordinary way: three failed attempts.
        h.latest().emitClose()
        vi.advanceTimersByTime(1_000)
        h.latest().emitClose()
        vi.advanceTimersByTime(2_000)
        h.latest().emitClose()
        expect(h.live.connection.value).toBe('reconnecting')
        const attempts = h.sockets.length

        h.win.emit('online')

        // Immediately, not after the 4 s the backoff had reached.
        expect(h.sockets).toHaveLength(attempts + 1)
        expect(h.live.connection.value).toBe('connecting')

        h.live.stop()
      } finally {
        vi.useRealTimers()
      }
    })
  })

  /**
   * The case neither `online` nor a foregrounding catches: the page stayed
   * visible and connected throughout, but the path underneath it died. Silence
   * is the only evidence, and silence only means something once the server has
   * shown it fills quiet periods with a beat.
   */
  describe('the heartbeat watchdog', () => {
    it('replaces a socket that goes silent after heartbeats started', () => {
      vi.useFakeTimers()
      try {
        const h = networkHarness()
        const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
        h.live.start()
        h.latest().emitOpen()
        h.latest().emitMessage(
          frame(HEARTBEAT, '00000000-0000-0000-0000-000000000000'),
        )

        vi.advanceTimersByTime(HEARTBEAT_TIMEOUT_MS - 1)
        expect(h.sockets).toHaveLength(1)

        vi.advanceTimersByTime(1)
        expect(h.sockets).toHaveLength(2)
        expect(h.live.connection.value).toBe('connecting')

        h.live.stop()
        warn.mockRestore()
      } finally {
        vi.useRealTimers()
      }
    })

    it('treats any frame as proof of life, not just the beat', () => {
      vi.useFakeTimers()
      try {
        const h = networkHarness()
        h.live.start()
        h.latest().emitOpen()
        h.latest().emitMessage(frame(HEARTBEAT))

        // Ordinary traffic keeps pushing the deadline out.
        for (let i = 0; i < 4; i += 1) {
          vi.advanceTimersByTime(HEARTBEAT_TIMEOUT_MS - 1_000)
          h.latest().emitMessage(frame(TIMELINE_EVENT))
        }

        expect(h.sockets).toHaveLength(1)

        h.live.stop()
      } finally {
        vi.useRealTimers()
      }
    })

    /**
     * The two halves of this fix ship as separate changes (one silo per PR), so
     * this client runs against a server that never beats. It must not decide
     * that a quiet account is a dead link and reconnect on a loop forever.
     */
    it('never arms against a server that sends no heartbeat', () => {
      vi.useFakeTimers()
      try {
        const h = networkHarness()
        h.live.start()
        h.latest().emitOpen()
        h.latest().emitMessage(frame(TIMELINE_EVENT))

        vi.advanceTimersByTime(HEARTBEAT_TIMEOUT_MS * 4)

        expect(h.sockets).toHaveLength(1)
        expect(h.live.connection.value).toBe('live')

        h.live.stop()
      } finally {
        vi.useRealTimers()
      }
    })

    it('re-disarms for each new socket', () => {
      vi.useFakeTimers()
      try {
        const h = networkHarness()
        h.live.start()
        h.latest().emitOpen()
        h.latest().emitMessage(frame(HEARTBEAT))
        h.latest().emitClose()
        vi.advanceTimersByTime(1_000)
        h.latest().emitOpen()

        // This socket's server has not beaten yet, so silence proves nothing.
        vi.advanceTimersByTime(HEARTBEAT_TIMEOUT_MS * 2)
        expect(h.live.connection.value).toBe('live')

        h.live.stop()
      } finally {
        vi.useRealTimers()
      }
    })

    it('does not route the beat to subscribers', () => {
      const h = networkHarness()
      const seen: LiveFrame[] = []
      h.live.subscribe((f) => seen.push(f))
      h.live.start()
      h.latest().emitOpen()
      h.latest().emitMessage(frame(HEARTBEAT))
      h.latest().emitMessage(frame(TIMELINE_EVENT))

      expect(seen.map((f) => f.type)).toEqual([TIMELINE_EVENT])

      h.live.stop()
    })
  })
})
