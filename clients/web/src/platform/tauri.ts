import { fetch as tauriFetch } from '@tauri-apps/plugin-http'
import WebSocketClient from '@tauri-apps/plugin-websocket'
import type { LiveSocket, Platform } from './index'

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
function adapt(connect: Promise<WebSocketClient>): LiveSocket {
  const socket: LiveSocket = {
    onopen: null,
    onmessage: null,
    onclose: null,
    onerror: null,
    close: () => {
      closed = true
      void connect.then((client) => client.disconnect()).catch(() => {})
    },
  }
  let closed = false

  void connect.then(
    (client) => {
      if (closed) {
        void client.disconnect().catch(() => {})
        return
      }
      client.addListener((message) => {
        if (message.type === 'Close') {
          socket.onclose?.(new CloseEvent('close'))
          return
        }
        if (message.type === 'Text') {
          socket.onmessage?.(
            new MessageEvent('message', { data: message.data }),
          )
        }
      })
      socket.onopen?.(new Event('open'))
    },
    () => {
      // A failed connection is an error then a close, as the DOM does it —
      // `live-connection.ts` drives its backoff off the close.
      socket.onerror?.(new Event('error'))
      socket.onclose?.(new CloseEvent('close'))
    },
  )

  return socket
}

export function tauriPlatform(): Platform {
  return {
    // The plugin's fetch is signature-compatible with the global.
    fetch: tauriFetch,
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
    // A packaged build has no same-origin API to assume: it must be told.
    defaultApiBaseUrl: null,
  }
}
