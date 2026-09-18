import { invoke } from '@tauri-apps/api/core'
import { getCurrentWebview } from '@tauri-apps/api/webview'
import { getCurrent, onOpenUrl } from '@tauri-apps/plugin-deep-link'
import { save } from '@tauri-apps/plugin-dialog'
import { writeFile } from '@tauri-apps/plugin-fs'
import { fetch as tauriFetch } from '@tauri-apps/plugin-http'
import { openUrl } from '@tauri-apps/plugin-opener'
import WebSocketClient from '@tauri-apps/plugin-websocket'
import { fileFromPath } from '../media/dropped-file'
import { MAX_UPLOAD_BYTES } from '../media/media-service'
import { basename } from '../media/filename'
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
    path = await save({ defaultPath: basename(file.filename) })
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

/**
 * The caller's own deadline, if it has one, plus the backstop.
 *
 * Composed rather than chosen. The obvious shape — leave a request alone if it
 * already carries a signal — cannot be written, because there is no way to ask
 * whether a `Request` carries one: `new Request(url)` with no `signal` option
 * still exposes a live `AbortSignal` that simply never fires, so "has a signal"
 * is true of every `Request` ever made. Reading it as consent to skip the
 * backstop exempted the whole openapi-fetch path, which is most of `/v1`, since
 * `openapi-fetch` calls its injected fetch with a `Request` it built itself.
 *
 * Composing needs no such question: whichever bound is shorter aborts first, so
 * the first-run health probe keeps its own few seconds and everything else
 * inherits the backstop.
 */
export function boundedSignal(
  input: RequestInfo | URL,
  init?: RequestInit,
  timeoutMs: number = REQUEST_TIMEOUT_MS,
): AbortSignal {
  const caller =
    init?.signal ?? (input instanceof Request ? input.signal : null)
  const backstop = AbortSignal.timeout(timeoutMs)
  return caller === null || caller === undefined
    ? backstop
    : AbortSignal.any([caller, backstop])
}

/**
 * How this shell identifies itself to the Axon authorization server.
 *
 * `clientId` and `redirectUri` are one registration, not two settings: the
 * server allow-lists URIs per client id, so `axon-desktop` with the browser's
 * callback — or `axon-web` with this one — is an unregistered *pair* and is
 * refused -- the server names which half is wrong, and logs the pair it was
 * sent (#399). The shell previously set
 * only the URI and kept the build-time `axon-web`, which is precisely that.
 *
 * The redirect is a reverse-domain scheme per RFC 8252 § 7.1 and ADR 0102 § 4,
 * not a short `axon:`. A private-use scheme is claimed first-come and
 * unauthenticated on every desktop OS, so a generic one is both easy to collide
 * with and easy to impersonate: any application registering `axon` could
 * receive an authorization code meant for this one (§ 8.4, § 8.6). Single
 * slash, because there is no authority component and `://` would make `oauth`
 * look like a host.
 *
 * It is *not* the scheme the bundle is served from. `APP_SCHEME` in
 * `src-tauri/src/lib.rs` stays `axon`: that is an in-webview protocol handler,
 * never registered with the OS, and takes no part in OAuth.
 *
 * The scheme must also match `plugins.deep-link.desktop.schemes` in
 * `tauri.conf.json`, and the operator's server needs the matching entry:
 *
 * ```toml
 * [[oauth.clients]]
 * client_id = "axon-desktop"
 * redirect_uris = ["org.matrixaxon.axon:/oauth/callback"]
 * ```
 */
const OAUTH_CLIENT = {
  clientId: 'axon-desktop',
  redirectUri: 'org.matrixaxon.axon:/oauth/callback',
}

/**
 * Read the files behind a set of dropped paths.
 *
 * `read_dropped_file` is this app's own command rather than the fs plugin's
 * `readFile`, and that is a deliberate narrowing. A dropped file can be
 * anywhere, so an fs-plugin route would need a scope wide enough to cover the
 * whole filesystem — a standing grant to read any file, held by the webview,
 * for the sake of a gesture. The command instead reads only paths the shell
 * has just seen the user drop on this window, so the grant lasts exactly as
 * long as the drag and covers exactly what was dragged.
 *
 * Sequential, not `Promise.all`: the batch is at most a handful of files
 * (`MAX_BATCH_FILES`), and staging order is the order they are sent in
 * (ADR 0081), so it should be the order the OS listed them.
 *
 * A path that cannot be read is skipped rather than failing the whole drop.
 * Dropping five images should not be lost to one of them being a broken
 * symlink, and the caller reports an empty result honestly.
 *
 * `MAX_UPLOAD_BYTES` goes over the bridge so the shell can refuse an oversized
 * file from its metadata instead of reading it. Staging applies the same limit
 * again on this side, which is the one that reports it to the user; this only
 * avoids spending a multi-gigabyte read to reach that verdict.
 */
async function readDroppedFiles(paths: readonly string[]): Promise<File[]> {
  const files: File[] = []
  for (const path of paths) {
    try {
      const bytes = await invoke<ArrayBuffer>('read_dropped_file', {
        path,
        maxBytes: MAX_UPLOAD_BYTES,
      })
      files.push(fileFromPath(path, bytes))
    } catch (error) {
      console.error('could not read a dropped file', path, error)
    }
  }
  return files
}

export function tauriPlatform(): Platform {
  return {
    // The plugin's fetch is signature-compatible with the global, but has no
    // timeout of its own; see `REQUEST_TIMEOUT_MS`. The signal has to go in
    // `init` and not on the `Request`: the plugin reads `init?.signal` alone
    // and never consults `input.signal`, so a bound left on the request object
    // is a bound it will never honour.
    fetch: (input, init) =>
      tauriFetch(input, { ...init, signal: boundedSignal(input, init) }),
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
    // The failure is returned, not swallowed. It used to be logged here and
    // discarded, which suited the link handler — the click is already
    // `preventDefault`ed, so there is nothing left to fall back to — and
    // stranded OAuth, where handing off to the browser *is* the sign-in and a
    // failure that never reaches the caller is a sign-in that silently never
    // begins. This is also exactly how a mis-scoped capability presents: the
    // opener denies every URL unless its scope says so, and the first version
    // of this granted the command without one.
    //
    // The message is rewritten rather than passed on. A link in a room can
    // carry a signed media URL or credentials in its query, and the plugin's
    // error text repeats the URL it was given — so forwarding it writes
    // secrets a user never chose to record into wherever the caller reports
    // errors. The origin is enough to tell a denied scope from an absent
    // handler, which is all anyone has ever needed from it.
    openExternal: (url) =>
      openUrl(url).catch(() => {
        throw new Error(`could not open an external link (${originOf(url)})`)
      }),
    oauthClient: OAUTH_CLIENT,
    onDeepLink: (handler) => {
      // Delivered once per URL, however many channels report it.
      //
      // The two below are not exclusive: on a cold launch the plugin sets the
      // value `getCurrent` returns *and* emits the event `onOpenUrl` listens
      // for — `handle_cli_arguments` does both on Windows and Linux, and
      // `RunEvent::Opened` does both on macOS. Today the emit happens during
      // plugin setup, before the webview has subscribed, so only `getCurrent`
      // reaches us; that is a matter of timing rather than of design, and
      // `tauri-plugin-single-instance` forwarding a URL into a running process
      // puts a second delivery on the same footing.
      //
      // A repeat is not harmless. `completeRedirect` consumes the pending PKCE
      // entry on success, so re-delivering a callback that just worked fails
      // its state check and paints "OAuth sign-in state did not match" over a
      // sign-in that succeeded a moment earlier — a bug the user cannot act on
      // and we would struggle to reproduce.
      const delivered = new Set<string>()
      const deliver = (raw: string) => {
        if (delivered.has(raw)) {
          return
        }
        delivered.add(raw)
        try {
          handler(new URL(raw))
        } catch {
          // The OS can hand us anything registered to the scheme; a URL we
          // cannot parse is not ours to act on.
        }
      }

      // The URL that *launched* this process, if any. `onOpenUrl` below only
      // reports links that arrive while the app is already up — on Windows and
      // Linux a cold launch carries the URL in argv instead, and the plugin
      // exposes it here rather than replaying it as an event. Without this, a
      // callback that starts the app (rather than returning to a running one)
      // is silently dropped: the app opens on the sign-in screen as if nothing
      // had happened.
      void getCurrent()
        .then((urls) => {
          for (const raw of urls ?? []) {
            deliver(raw)
          }
        })
        .catch(() => {})

      // `onOpenUrl` resolves to its own unlisten function; the subscription is
      // established asynchronously, so unsubscribing has to wait for it rather
      // than race it.
      const ready = onOpenUrl((urls) => {
        for (const raw of urls) {
          deliver(raw)
        }
      })
      return () => {
        void ready.then((unlisten) => unlisten()).catch(() => {})
      }
    },
    onNativeFileDrop: (handler) => {
      // Only ever fires where the shell left its drag-drop handler enabled,
      // which is Linux alone (`src-tauri/src/lib.rs`). Windows and macOS keep
      // the handler disabled so the page's own HTML5 events work — Tauri's own
      // docs require that on Windows — and there this subscription is simply
      // never called, which is why no platform check is needed here.
      const ready = getCurrentWebview().onDragDropEvent((event) => {
        const drag = event.payload
        if (drag.type === 'leave') {
          handler({ kind: 'leave' })
          return
        }
        // Used as-is, *not* run through `toLogical(devicePixelRatio)`, though
        // the payload types it as a physical position. On Linux — the only
        // platform this fires on — the number is what GTK handed wry from its
        // `drag-motion`/`drag-drop` signals (`wry/src/webkitgtk/drag_drop.rs`),
        // and GTK3 widget coordinates are already logical pixels; the runtime
        // wraps them in `PhysicalPosition` without multiplying by the scale
        // factor (`tauri-runtime-wry/src/lib.rs`). Dividing again would land a
        // drop at logical (800, 600) on a 2x display at (400, 300), in a
        // different pane or none, and the first version of this did exactly
        // that — unnoticed because it was only ever exercised at scale 1.
        const { x, y } = drag.position
        if (drag.type !== 'drop') {
          // `enter` and `over` are the same thing to a drop target: the cursor
          // is here, with a file.
          handler({ kind: 'over', x, y })
          return
        }
        // Read on demand and at most once, not eagerly. Every pane with a
        // composer subscribes here — the room and the thread panel at least —
        // and only the one under the cursor wants the bytes; a drop on the
        // sidebar is wanted by none of them. Reading up front would pull every
        // file over IPC once per subscriber and throw most of it away.
        let read: Promise<readonly File[]> | undefined
        handler({
          kind: 'drop',
          x,
          y,
          files: () => (read ??= readDroppedFiles(drag.paths)),
        })
      })
      // The subscription is established asynchronously, so unsubscribing has
      // to wait for it rather than race it.
      return () => {
        void ready.then((unlisten) => unlisten()).catch(() => {})
      }
    },
    // A packaged build has no same-origin API to assume: it must be told.
    defaultApiBaseUrl: null,
  }
}
