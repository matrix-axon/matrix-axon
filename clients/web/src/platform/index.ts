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

/** What `saveFile` is handed. */
export interface SaveRequest {
  blob: Blob
  filename: string
  /** Falls back to the blob's own type, then to a generic binary type. */
  mimetype?: string | null
}

/**
 * How a save ended. `shared` means a share sheet took the file rather than the
 * filesystem; `cancelled` means the user dismissed the sheet or dialog.
 */
export type SaveOutcome = 'saved' | 'shared' | 'cancelled' | 'failed'

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
   * Write a file to wherever this platform puts files.
   *
   * `<a download>` is the browser's answer and is *inert* from a custom
   * scheme — the shell has no download manager to honour it — so a packaged
   * build has to ask the OS for a path and write the bytes itself. Returning
   * the outcome rather than a boolean keeps "the user dismissed the save
   * dialog" distinguishable from "the save failed"; showing an error for
   * someone who simply changed their mind is worse than showing nothing.
   */
  saveFile(file: SaveRequest): Promise<SaveOutcome>

  /**
   * Open a link outside the app, or `null` when the anchor's own behaviour is
   * already right.
   *
   * `null` in a browser: `target="_blank"` opens a tab, and `window.open` is
   * banned repo-wide (`clients/web/AGENTS.md`), so there is nothing better to
   * do and intercepting would only break middle-click and modifiers. A shell
   * must supply one — an unhandled external link navigates the app window away
   * from the app, with no back button to return.
   */
  openExternal: ((url: string) => void) | null

  /**
   * How this build identifies itself to the Axon authorization server, or
   * `null` to use the build-time client id and a callback composed from the
   * page origin.
   *
   * One object because the two halves are one registration. The server
   * allow-lists redirect URIs *per client id*
   * (`OAuthClients::redirect_uri_allowed`), so a client id paired with the
   * wrong URI is not a partial configuration — it is an unregistered pair, and
   * `/v1/oauth/authorize` rejects it with "unknown client_id or redirect_uri".
   * Setting one without the other is exactly the shape of that mistake, so
   * they cannot be set separately.
   *
   * A shell's callback also cannot be composed from a base: resolving
   * `/oauth/callback` against `org.matrixaxon.axon:/oauth` yields
   * `org.matrixaxon.axon:/oauth/oauth/callback`. It is carried whole.
   */
  oauthClient: { clientId: string; redirectUri: string } | null

  /**
   * Subscribe to URLs the OS hands this app, or `null` where there is no such
   * channel.
   *
   * This is how the authorization code comes back: the sign-in happens in the
   * user's real browser (RFC 8252 — never in an embedded webview, which would
   * hand the app the user's IdP credentials), and the browser redirects to the
   * registered scheme, which the OS routes here. Returns an unsubscribe.
   */
  onDeepLink: ((handler: (url: URL) => void) => () => void) | null

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
    saveFile: saveInBrowser,
    // The anchor already does the right thing here; see `openExternal`.
    openExternal: null,
    // The build-time client id, and a callback composed from this origin.
    oauthClient: null,
    // A browser has no OS-level URL channel; the callback arrives as a
    // navigation to `/oauth/callback` instead.
    onDeepLink: null,
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

/**
 * How long the object URL is held after the anchor is clicked. Revoking
 * immediately races the browser's own read of the blob, and the download then
 * silently produces an empty file.
 */
const REVOKE_DELAY_MS = 60_000

/**
 * Offer the file to the platform share sheet, or `null` when there is no sheet
 * that takes files, so the caller falls back to the anchor.
 *
 * On a phone the anchor lands the file in Files rather than Photos, which reads
 * as the save having failed — so where the platform can share files, the sheet
 * is offered first and gives "Save Image" and AirDrop. Detected by capability,
 * never by user agent, and the anchor stays the path that must keep working.
 */
async function shareInBrowser(file: SaveRequest): Promise<SaveOutcome | null> {
  const share = navigator.share?.bind(navigator)
  const canShare = navigator.canShare?.bind(navigator)
  if (share === undefined || canShare === undefined) {
    return null
  }
  const shareable = new File([file.blob], file.filename, {
    type: file.mimetype ?? file.blob.type ?? 'application/octet-stream',
  })
  if (!canShare({ files: [shareable] })) {
    return null
  }
  try {
    await share({ files: [shareable] })
    return 'shared'
  } catch (error) {
    // Dismissing the sheet throws `AbortError`. That is a deliberate cancel,
    // not a failure — surfacing it as one would show an error for a user who
    // simply changed their mind. Anything else means the sheet could not take
    // the file, so fall back to the anchor rather than leave them with nothing.
    if (error instanceof DOMException && error.name === 'AbortError') {
      return 'cancelled'
    }
    return null
  }
}

/**
 * The browser save: share sheet where one takes files, transient anchor
 * otherwise. `window.open` is banned repo-wide, so there is no "open it in a
 * tab and let them save from there" fallback to lean on.
 */
async function saveInBrowser(file: SaveRequest): Promise<SaveOutcome> {
  const shared = await shareInBrowser(file)
  if (shared !== null) {
    return shared
  }
  const url = URL.createObjectURL(file.blob)
  const anchor = document.createElement('a')
  anchor.href = url
  anchor.download = file.filename
  document.body.appendChild(anchor)
  anchor.click()
  anchor.remove()
  setTimeout(() => URL.revokeObjectURL(url), REVOKE_DELAY_MS)
  return 'saved'
}
