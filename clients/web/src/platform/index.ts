/**
 * The platform seam (ADR 0102 § 2).
 *
 * Everything the client does that leaves the page goes through here, so a
 * packaged build can do it in the shell process instead of the webview. Today
 * that is transport only: the HTTP client, the media service, the OAuth token
 * exchange and the live socket. `browserPlatform()` is the web implementation
 * and is what every existing call site gets by default, so the browser build
 * behaves exactly as it did before this seam existed.
 *
 * Why the shell needs it at all: a packaged app loads from a custom scheme, so
 * every `/v1` call is cross-origin, and the server serves no CORS headers
 * (ADR 0046's M-W1.5 was designed and never built; ADR 0052 § 5 chose a
 * same-origin front door instead). Rather than make every self-hoster maintain
 * an origin allow-list — and still fail against the plain-http LAN server
 * `deploy/web/Caddyfile` explicitly supports, which a secure custom-scheme
 * origin refuses as mixed content — the shell routes this traffic through
 * Rust. ADR 0102 § 2 has the full reasoning.
 *
 * The Tauri implementation lands in M-W12 and is selected at runtime, so the
 * same `dist` still runs unmodified in a browser.
 */

/**
 * The subset of `WebSocket` the live connection actually uses
 * (`stores/live-connection.ts`): four handlers and `close()`. It never calls
 * `send()` — the frame protocol is server→client only (ADR 0020) — and never
 * reads `readyState`.
 *
 * Narrow on purpose. A real `WebSocket` satisfies it structurally, and a shell
 * transport that is not a `WebSocket` at all only has to produce these five
 * members rather than impersonate the whole DOM interface.
 */
export interface LiveSocket {
  onopen: ((event: Event) => void) | null
  onmessage: ((event: MessageEvent) => void) | null
  onclose: ((event: CloseEvent) => void) | null
  onerror: ((event: Event) => void) | null
  close(): void
}

export interface Platform {
  /**
   * The HTTP transport. Signature-compatible with the global, because
   * `openapi-fetch` takes it as its `fetch` option and the media service calls
   * it directly.
   */
  fetch: typeof globalThis.fetch

  /**
   * Open the live-event socket, authenticated with `token`.
   *
   * The *credential* is the contract here, not how it is carried. A browser
   * cannot set `Authorization` on an upgrade, so it smuggles the token through
   * `Sec-WebSocket-Protocol` (ADR 0029, `bearerSubprotocols` below) — but that
   * is a browser limitation, not a property of `/v1/ws`. A transport that can
   * set headers should send one, which is the branch
   * `crates/axon-api/src/ws.rs` tries *first* and the TUI already uses.
   *
   * Passing the encoded subprotocol list across this seam instead would bake
   * one platform's workaround into every platform, and oblige a shell to parse
   * `bearer.<token>` back apart to do the right thing.
   */
  openSocket(url: string, token: string): LiveSocket

  /**
   * The API base to fall back on when the user has configured none and no
   * `VITE_AXON_SERVER_URL` was baked in (ADR 0102 § 3).
   *
   * `'/'` in a browser, where the SPA and the API are served from one origin
   * by construction (`deploy/web/Caddyfile`) and so the question never needs
   * asking. `null` in a shell, which is distributed to people whose servers we
   * have never heard of and must therefore ask before it can do anything.
   */
  defaultApiBaseUrl: string | null
}

/**
 * The web implementation: the page's own `fetch` and `WebSocket`.
 *
 * `fetch` forwards rather than being captured, for two reasons. An unbound
 * `globalThis.fetch` reference throws "Illegal invocation" in a browser once
 * it is passed around as a value, which this seam does (into `openapi-fetch`,
 * into the media service) — and a `.bind()` would fix that but introduce a
 * subtler problem: it resolves the global *once*, at construction. The code
 * this replaced read the global on every call, and msw installs its
 * interceptor by swapping `globalThis.fetch`, so a service graph built before
 * `server.listen()` would hold the unintercepted function and quietly make
 * real network requests. Forwarding keeps the original late-binding
 * semantics, so "the browser build is unchanged" stays literally true.
 */
/**
 * The `Sec-WebSocket-Protocol` entries a browser offers: the benign `axon`
 * entry the server echoes to keep the 101 RFC 6455-compliant, then the
 * `bearer.<token>` entry carrying the credential (ADR 0029). `axon` is first so
 * the server has a non-secret protocol to negotiate; the token-bearing entry is
 * accepted but never echoed back.
 *
 * Lives here, beside the platform that needs it, rather than in `api/ws.ts`:
 * it is one platform's way of carrying a credential, not a property of the
 * `/v1/ws` protocol.
 */
export function bearerSubprotocols(token: string): string[] {
  return ['axon', `bearer.${token}`]
}

export function browserPlatform(): Platform {
  return {
    fetch: (...args) => globalThis.fetch(...args),
    openSocket: (url, token) => new WebSocket(url, bearerSubprotocols(token)),
    // Same-origin: the deployment that serves this bundle also proxies /v1.
    defaultApiBaseUrl: '/',
  }
}

/**
 * Whether this bundle is running inside the native shell.
 *
 * Feature-detected rather than compiled in, so one `dist` serves both targets —
 * ADR 0046's stated exit criterion for the shell, and what keeps the browser
 * build from needing a pipeline of its own.
 *
 * Kept synchronous, and kept separate from constructing the shell platform.
 * `main.tsx` branches on this and only then imports `./tauri`, so the plugin
 * code never enters the browser's boot path — which ADR 0085 and ADR 0087
 * measure, and which should not grow an `await` to support a target the
 * browser is not.
 */
export function isTauriRuntime(): boolean {
  return (
    typeof window !== 'undefined' &&
    '__TAURI_INTERNALS__' in (window as unknown as Record<string, unknown>)
  )
}
