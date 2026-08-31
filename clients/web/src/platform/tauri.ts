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
    saveFile: saveViaDialog,
    // Hand the link to the user's real browser. Left to itself, an external
    // anchor navigates the *app window* to that page, and the shell has no
    // back button to return with — the app is simply gone until restarted.
    openExternal: (url) => {
      void openUrl(url).catch(() => {})
    },
    // A packaged build has no same-origin API to assume: it must be told.
    defaultApiBaseUrl: null,
  }
}
