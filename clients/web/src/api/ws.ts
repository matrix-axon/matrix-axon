/**
 * `/v1/ws` URL + auth-subprotocol helpers (ADR 0046, M-W2; #238, M-W6).
 *
 * A browser `WebSocket` cannot set an `Authorization` header, so the token
 * rides in `Sec-WebSocket-Protocol` as a `bearer.<token>` entry (ADR 0029);
 * the server reads it there at upgrade time.
 *
 * The client also offers a benign, credential-free `axon` subprotocol *first*.
 * RFC 6455 §4.1 requires a client that offered subprotocols to fail the
 * connection when the server echoes none, and browsers enforce it (Chrome
 * hard, Firefox lenient) — so the server echoes `axon` in the 101 to keep the
 * handshake compliant, while never echoing the token-bearing entry (that would
 * leak the secret into response headers proxies and access logs may capture).
 * The server side landed in commit `3ee4c541`; this is its required client
 * half (`axon-api/src/ws.rs`, ADR 0029 amendment, #238) — mirrors the
 * Kubernetes API server's fix for the same problem.
 */

import { apiUrl } from '../server-url'
import {
  bearerSubprotocols,
  browserPlatform,
  type LiveSocket,
  type Platform,
} from '../platform'

/**
 * The only page protocols a live socket can be derived from. Anything else is
 * a caller error rather than a value to coerce — see `wsUrl`.
 */
const SOCKET_PROTOCOL: Readonly<Record<string, string>> = {
  'http:': 'ws:',
  'ws:': 'ws:',
  'https:': 'wss:',
  'wss:': 'wss:',
}

/**
 * The WebSocket URL for the API at `baseUrl` — same-origin by default (the
 * dev-proxy setup), or a cross-origin server root. `http(s)` maps to
 * `ws(s)`; an explicit `ws(s)` base passes through.
 *
 * `baseUrl` may be **relative**: `apiBaseUrl()` returns `'/'` for the
 * same-origin default, and that is the value the live socket is opened with.
 * It is resolved against the page origin first, because `new URL(path, base)`
 * throws on a relative base — a parameter default only covers `undefined`, so
 * an explicit `'/'` slipped past it and threw before any socket was
 * constructed, leaving the client permanently "reconnecting" (regression from
 * the `apiBaseUrl()` extraction in #349).
 *
 * **Anything that is not http(s)/ws(s) throws.** This used to fall through to
 * `ws:`, which was harmless while the page was always served over http(s) and
 * silently wrong the moment it was not. Under a packaged build the page origin
 * is the shell's own scheme, and a relative base resolved against it failed
 * two different ways, neither of them visibly:
 *
 * - `tauri://localhost` (macOS, iOS, Linux) → `tauri://localhost/v1/ws`. The
 *   coercion is a *no-op*, because setting `.protocol` on a non-special scheme
 *   is ignored per the URL spec. `new WebSocket()` then throws on the scheme.
 * - `http://tauri.localhost` (Windows, Android) → `ws://tauri.localhost/v1/ws`.
 *   A perfectly valid socket URL, pointing at the app's own origin instead of
 *   the server.
 *
 * Both land in `socketFactory`'s catch, which treats a throw as a failed
 * attempt and backs off — so the client reconnects forever with nothing in the
 * UI or the log to say why. The shell must pass an absolute server base;
 * failing loudly here is how it finds out it did not.
 */
export function wsUrl(baseUrl: string | URL = window.location.origin): string {
  const url = apiUrl('/v1/ws', baseUrl)
  const protocol = SOCKET_PROTOCOL[url.protocol]
  if (protocol === undefined) {
    throw new Error(
      `cannot derive a websocket URL from ${url.protocol} — ` +
        'the API base must be an absolute http(s) or ws(s) URL',
    )
  }
  url.protocol = protocol
  return url.toString()
}

/**
 * Re-exported for the tests and call sites that already name it. The list
 * itself now lives with the browser platform (`platform/index.ts`), because
 * carrying a credential in `Sec-WebSocket-Protocol` is that platform's
 * workaround rather than a property of `/v1/ws` (#238, ADR 0029).
 */
export { bearerSubprotocols as wsAuthProtocols }

/**
 * Open the live-event socket, authenticated with the caller's token.
 *
 * `platform` is the transport seam (ADR 0102 § 2): the browser opens a real
 * `WebSocket` and encodes the token as a subprotocol, while a packaged build
 * opens the socket in the shell process and can send a real `Authorization`
 * header. The token crosses this boundary as a token for exactly that reason.
 * Defaulted so every existing call site is unchanged.
 */
export function openLiveSocket(
  token: string,
  baseUrl?: string | URL,
  platform: Pick<Platform, 'openSocket'> = browserPlatform(),
): LiveSocket {
  return platform.openSocket(wsUrl(baseUrl), token)
}
