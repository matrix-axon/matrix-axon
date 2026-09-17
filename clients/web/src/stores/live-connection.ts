import { computed, signal, type ReadonlySignal } from '@preact/signals'
import { decodeFrame, HEARTBEAT, type LiveFrame } from '../api/frames'
import { perfMark } from '../perf'
import type { LiveSocket } from '../platform'

/**
 * The live socket's state, surfaced for the connection indicator (M-W6, ADR
 * 0061). `connecting` covers the initial handshake; `live` is an open socket;
 * `reconnecting` is the backoff window after an unexpected drop; `offline` is
 * the resting/torn-down state.
 */
export type ConnectionState = 'connecting' | 'live' | 'reconnecting' | 'offline'

/** A decoded-frame listener; return value ignored. */
export type FrameListener = (frame: LiveFrame) => void

/** First and maximum reconnect backoff, matching the TUI (`clients/tui/src/api.rs`). */
export const INITIAL_BACKOFF_MS = 1000
export const MAX_BACKOFF_MS = 30000
/**
 * Minimum uptime before a connection counts as healthy and resets the
 * backoff. Resetting on `open` alone turned a server that completes the
 * upgrade and then immediately closes (e.g. auth revoked post-handshake)
 * into a permanent 1 s open/close hammer with a gap-fill refetch per cycle.
 */
export const STABLE_CONNECTION_MS = 10000

/**
 * How long the socket may go silent, once the server has proved it heartbeats,
 * before the connection is treated as dead and replaced.
 *
 * A dropped socket normally announces itself: the transport fires `close` and
 * `scheduleReconnect` takes over. A network *handover* does not. When an
 * iPhone moves from WiFi to cell the old connection is left bound to an
 * interface that no longer has a route; neither peer sends a FIN or an RST, so
 * the socket sits in `OPEN` forever, `close` never fires, `connection` still
 * reads `live`, and every consumer waits for frames that cannot arrive. The
 * only thing that distinguishes that from an idle account is that the server's
 * beat stopped landing too.
 *
 * Set well above the server's cadence (`DEFAULT_WS_HEARTBEAT_INTERVAL` in
 * `crates/axon-api/src/state.rs`, 20 s) so an ordinary late beat — a slow link,
 * a busy server, a briefly throttled background tab — is never mistaken for a
 * dead path. Roughly two and a half beats have to go missing.
 */
export const HEARTBEAT_TIMEOUT_MS = 50000

/**
 * How long the page must have been hidden before becoming visible again counts
 * as "the network may have changed under us".
 *
 * `online` covers a handover the page was awake for, but not the common iOS
 * case: the phone switches networks while the app is backgrounded, the webview
 * is frozen so no event is ever delivered, and by the time it thaws
 * `navigator.onLine` has been `true` the whole time. Coming back to the
 * foreground is the only signal left. Brief flicks away are excluded because
 * replacing the socket costs every consumer a gap-fill refetch, and a two
 * second tab switch has not changed anyone's network.
 */
export const REVIVE_AFTER_HIDDEN_MS = 10000

export interface LiveConnection {
  /** The socket state, for the shell's connection indicator (step 4). */
  connection: ReadonlySignal<ConnectionState>
  /**
   * Increments each time the socket re-opens after a drop (never on the first
   * connect). Consumers effect on it to gap-fill the lossy bus — refetch the
   * open room's head, re-read verification flows and device state (ADR 0061).
   */
  reconnects: ReadonlySignal<number>
  /**
   * Register a decoded-frame listener; returns an unsubscribe. Every listener
   * sees every well-formed frame — consumers self-filter by `type`
   * (`timelineEvent(frame)`, …) and by `account_id`.
   */
  subscribe(listener: FrameListener): () => void
  /** Open the socket and keep it open (reconnecting on drops). Idempotent. */
  start(): void
  /** Tear the socket down and go `offline`; cancels any pending reconnect. */
  stop(): void
}

export interface LiveConnectionOptions {
  /**
   * Opens an authenticated socket (`openLiveSocket(token, base)` in
   * production). Injected so tests supply a hand-driven fake — jsdom has no
   * `WebSocket`. A factory that throws (e.g. no token) is treated as a failed
   * attempt: it backs off and retries while the connection is wanted.
   *
   * `LiveSocket` rather than `WebSocket`: this module only ever attaches the
   * four handlers and calls `close()`, and a packaged build's socket is opened
   * in the shell rather than by the webview (ADR 0102 § 2). A real `WebSocket`
   * satisfies it structurally.
   */
  socketFactory: () => LiveSocket
  /**
   * Where `online` is observed, and the clock the hidden-for measurement uses.
   * Injected so tests can drive a network change without a real browser; the
   * defaults are the globals.
   */
  window?: Pick<Window, 'addEventListener' | 'removeEventListener'>
  /** Where `visibilitychange` is observed, and what reports the hidden state. */
  document?: Pick<
    Document,
    'addEventListener' | 'removeEventListener' | 'hidden'
  >
  /** Injected for the hidden-for threshold; `Date.now` in production. */
  now?: () => number
}

/**
 * The single `/v1/ws` connection for the whole instance (M-W6, ADR 0061): the
 * bus is global and frames carry `account_id`, so one socket serves every
 * account (ADR 0020). It owns the socket, decodes each frame once, and fans it
 * out to registered listeners — the central router that keeps new frame kinds
 * off the transport path.
 *
 * On an unexpected drop it reconnects with exponential backoff (1 s → 30 s,
 * matching the TUI), resetting the delay once a connection stays up for
 * `STABLE_CONNECTION_MS`. Because the bus
 * has no resume cursor, a reconnect can only signal that frames may have been
 * missed — consumers watch `reconnects` and re-read (gap-fill).
 *
 * A `close` is not the only way a socket dies, though, and on a phone it is
 * not even the common one. Three further triggers replace the socket outright
 * (`revive`), because after a network handover the transport reports an open
 * connection that can no longer carry anything:
 *
 * - `online`, for a handover the page was awake to see;
 * - returning to the foreground after `REVIVE_AFTER_HIDDEN_MS`, for the
 *   handover that happened while the webview was frozen and delivered no
 *   event at all;
 * - `HEARTBEAT_TIMEOUT_MS` of silence once the server has shown it heartbeats,
 *   for everything neither of those catches.
 *
 * Each of those takes the same path as a real drop, `reconnects` included, so
 * the gap-fill consumers already implement is what repairs the stale state.
 */
export function createLiveConnection(
  options: LiveConnectionOptions,
): LiveConnection {
  const win = options.window ?? window
  const doc = options.document ?? document
  const clock = options.now ?? (() => Date.now())

  const connection = signal<ConnectionState>('offline')
  const reconnects = signal(0)
  const listeners = new Set<FrameListener>()

  let socket: LiveSocket | null = null
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null
  let backoffMs = INITIAL_BACKOFF_MS
  /** When the current socket opened, for the stable-uptime backoff reset. */
  let openedAtMs: number | null = null
  /** True once any socket has opened this session — distinguishes reconnect. */
  let everConnected = false
  /**
   * The last message `socketFactory` threw, so a permanent failure is reported
   * once instead of on every retry. Cleared on a successful open, so a fault
   * that recurs after a good connection is reported again.
   */
  let lastFactoryError: string | null = null
  /** True between `start()` and `stop()` — whether a connection is wanted. */
  let wanted = false
  /** The silence watchdog, armed only once a heartbeat has been seen. */
  let heartbeatTimer: ReturnType<typeof setTimeout> | null = null
  /**
   * Whether the *current* socket's server has sent a heartbeat. Per socket, not
   * per session: a server that does not beat must never be force-reconnected
   * every window, so the watchdog stays disarmed until the server proves it
   * beats. That makes this client safe against an older server, which matters
   * because the two halves ship as separate changes.
   */
  let heartbeating = false
  /** When the page went hidden, for the `REVIVE_AFTER_HIDDEN_MS` threshold. */
  let hiddenAt: number | null = null

  function dispatch(frame: LiveFrame): void {
    for (const listener of listeners) {
      // One misbehaving listener must not starve the others or drop the frame.
      try {
        listener(frame)
      } catch {
        // Swallow: a consumer bug is not the transport's problem.
      }
    }
  }

  /** Stop the silence watchdog; it re-arms on the next heartbeat. */
  function disarmWatchdog(): void {
    if (heartbeatTimer !== null) {
      clearTimeout(heartbeatTimer)
      heartbeatTimer = null
    }
  }

  /**
   * Note that something arrived from the server.
   *
   * *Any* frame proves the path is alive and pushes the deadline out — a busy
   * room keeps the socket healthy without waiting on a beat. Only a heartbeat
   * may *arm* the watchdog, though: until one has landed, this client has no
   * evidence the server sends them, and arming on ordinary traffic would make
   * a quiet hour on a pre-heartbeat server look identical to a dead link.
   */
  function noteTraffic(type: string | null): void {
    if (!heartbeating) {
      if (type !== HEARTBEAT) {
        return
      }
      heartbeating = true
    }
    disarmWatchdog()
    heartbeatTimer = setTimeout(() => {
      heartbeatTimer = null
      console.warn(
        'live connection: no server heartbeat for',
        `${HEARTBEAT_TIMEOUT_MS}ms — replacing the socket`,
      )
      revive()
    }, HEARTBEAT_TIMEOUT_MS)
  }

  /** Detach and close a socket without letting its `close` drive the state. */
  function discard(closing: LiveSocket | null): void {
    if (closing === null) {
      return
    }
    closing.onclose = null
    closing.onmessage = null
    closing.onopen = null
    closing.onerror = null
    closing.close()
  }

  /**
   * Replace the socket now, on the suspicion that the one we have is dead.
   *
   * The three callers — `online`, a long-hidden page coming back, and the
   * silence watchdog — share a problem the transport cannot report: after a
   * network handover the socket is still `OPEN` and still useless. `readyState`
   * is no help, so this does not consult it; it closes whatever is there and
   * opens a fresh one unconditionally.
   *
   * `everConnected` is deliberately left alone, so the new socket's `open`
   * bumps `reconnects` and every consumer gap-fills (ADR 0061) — which is the
   * actual repair. The backoff is reset because an external event arriving is
   * new information, not another failed attempt.
   */
  function revive(): void {
    if (!wanted) {
      return
    }
    disarmWatchdog()
    if (socket !== null) {
      perfMark('live:close')
    }
    const closing = socket
    socket = null
    openedAtMs = null
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer)
      reconnectTimer = null
    }
    backoffMs = INITIAL_BACKOFF_MS
    connection.value = 'connecting'
    discard(closing)
    openSocket()
  }

  const onOnline = () => {
    revive()
  }

  const onVisibilityChange = () => {
    if (doc.hidden) {
      hiddenAt = clock()
      return
    }
    const awayFor = hiddenAt === null ? 0 : clock() - hiddenAt
    hiddenAt = null
    if (awayFor >= REVIVE_AFTER_HIDDEN_MS) {
      revive()
    }
  }

  /** Back off, then reconnect — as long as a connection is still wanted. */
  function scheduleReconnect(): void {
    if (!wanted) {
      connection.value = 'offline'
      return
    }
    connection.value = 'reconnecting'
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null
      openSocket()
    }, backoffMs)
    backoffMs = Math.min(backoffMs * 2, MAX_BACKOFF_MS)
  }

  function openSocket(): void {
    // A closed socket keeps its handlers (the stale-close check below needs
    // them), so a frame landing after its `close` can leave a watchdog armed
    // for a socket that no longer exists. Clearing it here means no timer ever
    // outlives the socket that armed it and fires into this one's window.
    disarmWatchdog()
    let opened: LiveSocket
    try {
      opened = options.socketFactory()
    } catch (error) {
      // No socket to attach to: treat as a failed attempt and back off.
      //
      // But say so once. Two very different things arrive here — "no token
      // yet", which is ordinary and resolves itself at sign-in, and "this base
      // URL cannot produce a websocket URL", which is a configuration error
      // that will never resolve. Both used to be swallowed, so the second
      // presented as a permanent "reconnecting" with nothing anywhere to say
      // why; `api/ws.ts` throws a description of exactly that case and had
      // nowhere to put it.
      //
      // Logged once per distinct message rather than on every attempt,
      // because the retry loop is unbounded and would otherwise turn a real
      // diagnostic into scroll.
      const message = error instanceof Error ? error.message : String(error)
      if (message !== lastFactoryError) {
        lastFactoryError = message
        console.warn('live connection: could not open a socket —', message)
      }
      socket = null
      scheduleReconnect()
      return
    }
    lastFactoryError = null
    socket = opened
    opened.onopen = () => {
      openedAtMs = Date.now()
      heartbeating = false
      // The room-open readout counts these: a reconnect mid-open re-issues
      // the room's head load (ADR 0061 gap-fill).
      perfMark('live:open', { reconnect: everConnected })
      if (everConnected) {
        reconnects.value += 1
      }
      everConnected = true
      connection.value = 'live'
    }
    opened.onmessage = (event) => {
      if (typeof event.data !== 'string') {
        return
      }
      const frame = decodeFrame(event.data)
      // Before the null check: a frame this client cannot parse is still proof
      // that the connection carries bytes, which is all the watchdog asks.
      noteTraffic(frame?.type ?? null)
      if (frame === null || frame.type === HEARTBEAT) {
        return
      }
      dispatch(frame)
    }
    opened.onclose = () => {
      // Ignore a stale socket's close (one replaced by stop() or a reconnect).
      if (socket === opened) {
        perfMark('live:close')
        disarmWatchdog()
        // Only a connection that stayed up counts as recovery; an
        // accept-then-close cycle keeps growing the backoff.
        if (
          openedAtMs !== null &&
          Date.now() - openedAtMs >= STABLE_CONNECTION_MS
        ) {
          backoffMs = INITIAL_BACKOFF_MS
        }
        openedAtMs = null
        socket = null
        scheduleReconnect()
      }
    }
    // A failed connection fires `error` then `close`; the close handler drives
    // the transition, so `error` needs no separate handling here.
    opened.onerror = () => {}
  }

  function start(): void {
    if (wanted) {
      return
    }
    wanted = true
    everConnected = false
    backoffMs = INITIAL_BACKOFF_MS
    hiddenAt = doc.hidden ? clock() : null
    // Bound to the socket's own lifetime rather than mounted by a component:
    // these exist to repair *this* socket, and a listener outliving it would
    // revive a connection nobody wants.
    win.addEventListener('online', onOnline)
    doc.addEventListener('visibilitychange', onVisibilityChange)
    connection.value = 'connecting'
    openSocket()
  }

  function stop(): void {
    wanted = false
    everConnected = false
    backoffMs = INITIAL_BACKOFF_MS
    hiddenAt = null
    heartbeating = false
    win.removeEventListener('online', onOnline)
    doc.removeEventListener('visibilitychange', onVisibilityChange)
    disarmWatchdog()
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer)
      reconnectTimer = null
    }
    const closing = socket
    socket = null
    connection.value = 'offline'
    discard(closing)
  }

  return {
    connection: computed(() => connection.value),
    reconnects: computed(() => reconnects.value),
    subscribe(listener) {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
    start,
    stop,
  }
}
