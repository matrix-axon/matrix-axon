import { computed, signal, type ReadonlySignal } from '@preact/signals'
import { decodeFrame, type LiveFrame } from '../api/frames'
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
 */
export function createLiveConnection(
  options: LiveConnectionOptions,
): LiveConnection {
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

  function dispatch(raw: string): void {
    const frame = decodeFrame(raw)
    if (frame === null) {
      return
    }
    for (const listener of listeners) {
      // One misbehaving listener must not starve the others or drop the frame.
      try {
        listener(frame)
      } catch {
        // Swallow: a consumer bug is not the transport's problem.
      }
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
      if (everConnected) {
        reconnects.value += 1
      }
      everConnected = true
      connection.value = 'live'
    }
    opened.onmessage = (event) => {
      if (typeof event.data === 'string') {
        dispatch(event.data)
      }
    }
    opened.onclose = () => {
      // Ignore a stale socket's close (one replaced by stop() or a reconnect).
      if (socket === opened) {
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
    connection.value = 'connecting'
    openSocket()
  }

  function stop(): void {
    wanted = false
    everConnected = false
    backoffMs = INITIAL_BACKOFF_MS
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer)
      reconnectTimer = null
    }
    const closing = socket
    socket = null
    connection.value = 'offline'
    if (closing !== null) {
      closing.onclose = null
      closing.onmessage = null
      closing.onopen = null
      closing.onerror = null
      closing.close()
    }
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
