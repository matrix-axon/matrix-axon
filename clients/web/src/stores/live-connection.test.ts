import { describe, expect, it, vi } from 'vitest'
import { TIMELINE_EVENT, type LiveFrame } from '../api/frames'
import { FakeWebSocket } from '../test/fake-socket'
import { createLiveConnection, STABLE_CONNECTION_MS } from './live-connection'

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

describe('createLiveConnection', () => {
  it('starts offline, goes connecting on start, then live on open', () => {
    const { live, socket } = harness()
    expect(live.connection.value).toBe('offline')
    live.start()
    expect(live.connection.value).toBe('connecting')
    socket().emitOpen()
    expect(live.connection.value).toBe('live')
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
})
