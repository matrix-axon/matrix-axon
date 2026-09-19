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

/**
 * One stage of a drag the OS reported to the window rather than to the page.
 *
 * `x`/`y` are CSS pixels in the webview's own viewport coordinates, so they can
 * be handed straight to `document.elementFromPoint`. Whatever unit the OS
 * reports in, the platform is responsible for delivering this one; on Linux,
 * the only platform with this channel, GTK already reports logical pixels and
 * the platform passes them through (`platform/tauri.ts`).
 */
export type NativeDrag =
  /** The cursor is over the window with a drag in progress. */
  | { kind: 'over'; x: number; y: number }
  /** The drag left the window, or was cancelled. It has no position. */
  | { kind: 'leave' }
  /**
   * `files` reads the dropped files, once, and hands back the same promise
   * to every caller. It is a function rather than a value so that only the
   * pane the drop actually landed on pays for the read: every composer
   * subscribes to this channel, and a drop on the sidebar is wanted by none of
   * them. The result is empty when every path failed to read, which is a real
   * outcome worth reporting rather than a reason to stay silent.
   */
  | {
      kind: 'drop'
      x: number
      y: number
      files: () => Promise<readonly File[]>
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
   *
   * Resolves once the link has been handed to the browser, and **rejects if it
   * could not be**. A caller that only wants the link opened may discard the
   * promise; OAuth cannot, because handing off is the whole of `startSignIn`
   * and a failure there is a sign-in that silently never begins.
   */
  openExternal: ((url: string) => Promise<void>) | null

  /**
   * How this build identifies itself to the Axon authorization server, or
   * `null` to use the build-time client id and a callback composed from the
   * page origin.
   *
   * One object because the two halves are one registration. The server
   * allow-lists redirect URIs *per client id*
   * (`OAuthClients::redirect_uri_allowed`), so a client id paired with the
   * wrong URI is not a partial configuration — it is an unregistered pair, and
   * `/v1/oauth/authorize` rejects it with "unknown client_id" or
   * "redirect_uri is not registered for this client_id".
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
   * Subscribe to file drags the *OS* delivers to this window, or `null` where
   * the webview's own HTML5 drag-and-drop already works.
   *
   * Not a nicety: on Linux there is no other channel. WebKitGTK hands the page
   * a `text/uri-list` and no `File` for a file-manager drag, so
   * `dataTransfer.files` is empty and there is nothing to stage — the symptom
   * reported was a drop that was accepted and then did nothing. The shell takes
   * the drag at the window instead and reports the *paths*, which it can read.
   *
   * The event carries a viewport point rather than a target element, because
   * it never went through the DOM. `media/use-file-drop.ts` hit-tests it, which
   * is what keeps a drop on the thread panel out of the room's composer
   * (ADR 0065) now that the event no longer knows what it landed on.
   *
   * Returns an unsubscribe.
   */
  onNativeFileDrop: ((handler: (drag: NativeDrag) => void) => () => void) | null

  /**
   * Whether reloading could produce a different build.
   *
   * `true` in a browser, which is the premise the whole update path rests on
   * (ADR 0087): the origin serves the bundle, a deploy replaces it, so
   * `version.json` can disagree with `BUILD_INFO` and a reload picks the new
   * one up.
   *
   * `false` in a packaged build, where `dist` is compiled into the binary. The
   * manifest it would fetch is the one it shipped with — served by the shell's
   * own scheme handler out of the same bundle — so the comparison is an
   * identity check that can only ever answer "current", and a reload brings
   * back exactly what was already running. Polling it costs a timer and a
   * request to say nothing, "Check for updates" asserts a currency the build
   * cannot know, and the banner would offer a Reload that cannot deliver.
   *
   * The desktop updater (ADR 0102 § 8) replaces this with a real check when
   * there are desktop artifacts to update *to*; until then the honest answer
   * is to say nothing rather than something false.
   */
  updatesFromOrigin: boolean

  /**
   * Whether the browser can adopt this page as an app of its own: add it to a
   * home screen, or register it as the handler for a URL scheme.
   *
   * `true` in a browser, where both are things the user may want and only the
   * browser can grant. `false` in a packaged shell, where the OS has already
   * installed the app — there is no page for a browser to adopt, `matrix:` is
   * claimed by the bundle rather than at runtime, and the APIs behind both
   * (`beforeinstallprompt`, `navigator.registerProtocolHandler`) do not exist
   * in a webview at all.
   *
   * Without this the settings for them still render, disabled, explaining that
   * "this browser" does not support something — in an application that is not
   * a browser and would not use the answer.
   */
  browserCanAdoptApp: boolean

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
    // The page's own drag-and-drop events are the channel here, and they carry
    // the files. Nothing to add.
    onNativeFileDrop: null,
    // Same-origin: the deployment that serves this bundle also proxies /v1.
    defaultApiBaseUrl: '/',
    // A deploy replaces what this origin serves, which is what makes the
    // version manifest, the banner and the auto-reload meaningful.
    updatesFromOrigin: true,
    // A page in a browser: installable to a home screen, and able to offer
    // itself as a `matrix:` handler.
    browserCanAdoptApp: true,
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
