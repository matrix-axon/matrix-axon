import { save } from '@tauri-apps/plugin-dialog'
import { writeFile } from '@tauri-apps/plugin-fs'
import { fetch as tauriFetch } from '@tauri-apps/plugin-http'
import { openUrl } from '@tauri-apps/plugin-opener'
import WebSocketClient from '@tauri-apps/plugin-websocket'
import type { LiveSocket, Platform, SaveOutcome, SaveRequest } from './index'

/**
 * The packaged-build platform (ADR 0102 § 2).
 *
 * Both transports run in the shell process rather than the webview, which is
 * the whole point: the page loads from a custom scheme, so a webview `fetch`
 * at the user's server would be a cross-origin request against a server that
 * sends no CORS headers, and a webview reaching a plain-http LAN server from a
 * secure origin would be blocked as mixed content besides. Going through Rust
 * means self-hosters configure nothing and a LAN server just works.
 */

/**
 * How long a shell request may run before it is abandoned.
 *
 * A backstop, not a latency target — generous enough that a large attachment on
 * a slow link finishes, short enough that a blackholed server cannot hold
 * resources for the life of the session. A browser applies its own network
 * timeouts; `reqwest` behind the http plugin applies none, so without this a
 * request to a host that accepts the connection and never answers simply never
 * settles. `media-service.ts` runs downloads through a bounded permit pool, so
 * such a request does not merely hang itself: it retains a permit, and enough
 * of them stall media for everything else.
 */
const REQUEST_TIMEOUT_MS = 120_000

/**
 * How long to wait for the live socket to connect.
 *
 * Shorter, because this is a handshake rather than a transfer, and because the
 * cost of giving up is small: `live-connection.ts` backs off and retries. With
 * no bound, a blackholed server left the connection reported as `connecting`
 * for the rest of the session, never failing and so never retrying.
 */
const SOCKET_CONNECT_TIMEOUT_MS = 20_000

/**
 * Adapt the plugin's socket to the four handlers `live-connection.ts` uses.
 *
 * The plugin delivers messages through one `addListener` callback and has no
 * DOM events, so the handlers are invoked with synthesized ones. Only `data`
 * is ever read (`typeof event.data === 'string'`), so the rest of a real
 * `MessageEvent` does not need faking.
 *
 * Connecting is async while `new WebSocket()` is not, so this returns the
 * socket object immediately and fires `onopen` once the connection lands —
 * which is the same ordering the DOM gives, where the constructor returns
 * before the handshake completes.
 */
export function adapt(
  connect: Promise<WebSocketClient>,
  timeoutMs: number = SOCKET_CONNECT_TIMEOUT_MS,
): LiveSocket {
  const socket: LiveSocket = {
    onopen: null,
    onmessage: null,
    onclose: null,
    onerror: null,
    close: () => {
      closed = true
      clearTimeout(timer)
      void connect.then((client) => client.disconnect()).catch(() => {})
    },
  }
  let closed = false
  let ended = false

  // The plugin's `connect` awaits the handshake with no bound of its own, so a
  // host that accepts the TCP connection and then says nothing leaves this
  // promise pending forever. `closed` is set alongside the failure so that a
  // connection which does eventually arrive is disconnected by the branch
  // below rather than resurrecting a socket the caller has been told is dead.
  const timer = setTimeout(() => {
    closed = true
    fail(`websocket did not connect within ${String(timeoutMs)}ms`)
  }, timeoutMs)

  /** Close once, whatever ends the socket — `live-connection` backs off on it. */
  function finish(): void {
    if (ended) {
      return
    }
    ended = true
    socket.onclose?.(new CloseEvent('close'))
  }

  /** A fatal condition: error then close, the order the DOM uses. */
  function fail(reason: string): void {
    if (ended) {
      return
    }
    console.warn(reason)
    socket.onerror?.(new Event('error'))
    finish()
  }

  void connect.then(
    (client) => {
      clearTimeout(timer)
      if (closed) {
        void client.disconnect().catch(() => {})
        return
      }
      client.addListener((message) => {
        // The plugin forwards whatever its Rust side puts on the channel. A
        // read failure — a reset connection, a server that went away — arrives
        // as the error *serialised to a string*, and the read loop then ends
        // without sending `Close`. Anything not recognised as a frame is
        // therefore treated as fatal: ignoring it left a dead socket reported
        // as `live` for the rest of the session, with the UI insisting it was
        // connected and no reconnection ever attempted.
        if (
          typeof message !== 'object' ||
          message === null ||
          !('type' in message)
        ) {
          fail(`websocket read failed: ${String(message)}`)
          return
        }
        switch (message.type) {
          case 'Close':
            finish()
            return
          case 'Text':
            socket.onmessage?.(
              new MessageEvent('message', { data: message.data }),
            )
            return
          // Real frames the live protocol does not use (ADR 0020 is
          // server→client text). Not errors; nothing to do with them.
          case 'Binary':
          case 'Ping':
          case 'Pong':
            return
          default:
            // Unreachable per the plugin's declared `Message` union, and that
            // is exactly the assumption being defended: the union describes
            // what the Rust side is *supposed* to send.
            fail(
              `websocket sent an unknown frame: ${String(
                (message as { type?: unknown }).type,
              )}`,
            )
        }
      })
      socket.onopen?.(new Event('open'))
    },
    () => {
      clearTimeout(timer)
      // A failed connection is an error then a close, as the DOM does it —
      // `live-connection.ts` drives its backoff off the close.
      fail('websocket could not connect')
    },
  )

  return socket
}

/**
 * Ask the OS where to put the file, then write it.
 *
 * `<a download>` cannot do this from a custom scheme — the shell has no
 * download manager — so a packaged build that kept the browser path would show
 * a Download button that silently did nothing.
 *
 * `save()` resolves to `null` when the dialog is dismissed, which is a cancel
 * and not a failure: the caller shows an error for `'failed'`, and someone who
 * changed their mind should not see one.
 */
async function saveViaDialog(file: SaveRequest): Promise<SaveOutcome> {
  let path: string | null
  try {
    path = await save({ defaultPath: file.filename })
  } catch {
    return 'failed'
  }
  if (path === null) {
    return 'cancelled'
  }
  try {
    await writeFile(path, new Uint8Array(await file.blob.arrayBuffer()))
    return 'saved'
  } catch {
    return 'failed'
  }
}

/** `https://host:port` for logging, or a placeholder if it will not parse. */
function originOf(url: string): string {
  try {
    return new URL(url).origin
  } catch {
    return '(unparseable url)'
  }
}

export function tauriPlatform(): Platform {
  return {
    // The plugin's fetch is signature-compatible with the global, but has no
    // timeout of its own; see `REQUEST_TIMEOUT_MS`. A caller that supplies its
    // own signal keeps it — the first-run health probe wants a much shorter
    // bound than this, and overriding it would make the setup screen hang.
    fetch: (input, init) => {
      const caller =
        init?.signal ?? (input instanceof Request ? input.signal : null)
      if (caller !== null && caller !== undefined) {
        return tauriFetch(input, init)
      }
      return tauriFetch(input, {
        ...init,
        signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      })
    },
    openSocket: (url, token) =>
      adapt(
        // A real `Authorization` header, not the `Sec-WebSocket-Protocol`
        // smuggling a browser is forced into (ADR 0029). This socket is opened
        // outside the webview, so the limitation does not apply — and the
        // header is the branch `crates/axon-api/src/ws.rs` tries first, the
        // same one the TUI uses. The plugin's ConnectionConfig has no
        // `protocols` field anyway; it takes headers.
        WebSocketClient.connect(url, {
          headers: { Authorization: `Bearer ${token}` },
        }),
      ),
    saveFile: saveViaDialog,
    // Hand the link to the user's real browser. Left to itself, an external
    // anchor navigates the *app window* to that page, and the shell has no
    // back button to return with — the app is simply gone until restarted.
    openExternal: (url) => {
      // Not swallowed. The click has already been `preventDefault`ed — letting
      // the navigation through instead would take the app window to the page
      // and there is no way back — so a rejection here means the user clicked
      // a link and nothing happened, with the reason known only to us. It is
      // also exactly how a mis-scoped capability presents: the opener denies
      // every URL unless its scope says otherwise, and the first version of
      // this granted the command without one.
      void openUrl(url).catch(() => {
        // Origin only, and no raw error. A link in a room can carry a signed
        // media URL or credentials in its query, and the plugin's error text
        // repeats the URL it was given — so logging either writes secrets a
        // user never chose to record into a file they may well attach to a bug
        // report. The origin is enough to tell a denied scope from an absent
        // handler, which is all this line was ever for.
        console.error('could not open an external link', originOf(url))
      })
    },
    // A packaged build has no same-origin API to assume: it must be told.
    defaultApiBaseUrl: null,
  }
}
