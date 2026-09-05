import { STORAGE_KEY as TOKEN_KEY } from './auth/token-paste'
import {
  PENDING_KEY as OAUTH_PENDING_KEY,
  SESSION_KEY as OAUTH_SESSION_KEY,
} from './auth/oauth'
/**
 * Which Axon server this client talks to (ADR 0102 § 3).
 *
 * A browser deployment answers this by construction: the SPA and the API are
 * served from one origin (`deploy/web/Caddyfile`), so the base is `/` and
 * nobody is ever asked. A packaged build cannot — it is distributed through a
 * store or a download page to people whose servers we have never heard of, and
 * `VITE_AXON_SERVER_URL` is baked in at build time. So the base becomes a
 * runtime setting, resolved in this order:
 *
 *   1. what the user configured and we persisted
 *   2. `VITE_AXON_SERVER_URL`, the build-time default
 *   3. the platform's own default — `/` in a browser, nothing in a shell
 *
 * `null` from `resolveApiBaseUrl` means "we do not know yet, ask", which is
 * only reachable on a platform with no same-origin API to fall back to.
 *
 * Kept out of `stores/settings.ts` deliberately: that store is created *inside*
 * `createServices()`, and this value is needed to build the graph in the first
 * place.
 */

/** Its own key, not part of the `axon.settings` envelope — see above. */
export const SERVER_URL_KEY = 'axon.server'

/**
 * Parse what a human typed into a base URL, or `null` if it cannot be one.
 *
 * Deliberately forgiving about the scheme, because the value is typed by hand
 * on a phone keyboard as often as not: a bare `axon.example.com` becomes
 * `https://axon.example.com`. Deliberately strict about everything else — a
 * `javascript:` or `data:` entry here would be handed to `fetch` and used to
 * build a WebSocket URL, so only http and https are accepted.
 *
 * A path is preserved (some deployments live under `/axon`), but a trailing
 * slash is stripped so the result concatenates cleanly with `/v1/...` — the
 * same normalisation `createMediaService` already does to its `baseUrl`.
 */
/**
 * Join an absolute `/v1/...` path onto a base that may carry a path prefix.
 *
 * `new URL('/v1/ws', base)` is the obvious thing and is wrong: a leading slash
 * resolves against the *origin*, silently discarding the `/axon` in
 * `https://example.com/axon`. `normalizeServerUrl` promises prefixes are
 * preserved, and the API client and media service honour that by
 * concatenating — this is how the socket and the OAuth endpoints do too, so
 * one contract holds everywhere instead of two.
 */
export function apiUrl(path: string, baseUrl: string | URL): URL {
  const base = new URL(String(baseUrl), window.location.origin)
  const prefix = base.pathname.replace(/\/+$/, '')
  return new URL(`${prefix}${path}`, base)
}

export function normalizeServerUrl(raw: string): string | null {
  const trimmed = raw.trim()
  if (trimmed === '') {
    return null
  }
  // A scheme is detected by `://`, not by a bare colon. `host:port` is the
  // most natural thing to type for a LAN server, and `localhost:8080` matches
  // "scheme followed by colon" exactly — which rejected it as an unsupported
  // `localhost:` scheme. `//host` is protocol-relative, not a scheme, so it is
  // a bare host too.
  const withScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(trimmed)
    ? trimmed
    : `https://${trimmed.replace(/^\/+/, '')}`

  let url: URL
  try {
    url = new URL(withScheme)
  } catch {
    return null
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    return null
  }
  if (url.hostname === '') {
    return null
  }
  // No credentials in a base URL. They would ride into every request path and
  // into the ADR 0085 cache namespace, and this is reachable by accident now
  // that a bare colon no longer reads as a scheme: `mailto:a@b.c` becomes
  // `https://mailto:a@b.c`, which parses cleanly as user `mailto`, password
  // `a`, host `b.c`.
  if (url.username !== '' || url.password !== '') {
    return null
  }
  // Query and fragment are meaningless on a base and would corrupt every
  // path built from it.
  url.search = ''
  url.hash = ''
  return url.toString().replace(/\/$/, '')
}

/**
 * The stored server URL, or `null`. Re-normalised on read: the value may have
 * been written by an older build, or edited by hand in devtools.
 *
 * Storage access is wrapped because it throws outright in some privacy modes,
 * and a client that cannot read a preference should still start.
 */
export function readStoredServerUrl(storage: Storage): string | null {
  let stored: string | null
  try {
    stored = storage.getItem(SERVER_URL_KEY)
  } catch {
    return null
  }
  return stored === null ? null : normalizeServerUrl(stored)
}

/** Persist a server URL. Returns the normalised value, or `null` if invalid. */
export function storeServerUrl(storage: Storage, raw: string): string | null {
  const normalized = normalizeServerUrl(raw)
  if (normalized === null) {
    return null
  }
  try {
    storage.setItem(SERVER_URL_KEY, normalized)
  } catch {
    // Unwritable storage is not a reason to refuse the connection; it just
    // means the choice does not survive a restart.
  }
  return normalized
}

/** Forget the configured server, sending the app back to the setup screen. */
export function clearStoredServerUrl(storage: Storage): void {
  try {
    storage.removeItem(SERVER_URL_KEY)
  } catch {
    // Nothing to undo.
  }
}

/**
 * Forget the server *and* the credentials held for it.
 *
 * Credentials are stored under fixed keys — `axon.token`,
 * `axon.oauth.session`, `axon.oauth.pending` — which say nothing about which
 * server issued them. Clearing only the URL would leave server A's bearer or
 * refresh token in place and send it to server B on the next request: a token
 * handed to a host that was never meant to see it, and one the user has no way
 * to notice.
 *
 * Both tiers are cleared, because `createAuthPersistence` writes to
 * `localStorage` or `sessionStorage` depending on "remember me" and a switch
 * must not depend on which was used.
 *
 * The alternative is to namespace credentials by normalized server URL, the
 * way `cacheNamespace()` already keys the durable cache (ADR 0085). That would
 * keep sessions for several servers alive at once; it is more machinery than
 * M-W12 needs, and it is a persistence-schema change with a migration for
 * everyone who already has an `axon.token`. Deliberately not taken here — the
 * user switching servers expects to sign in again, and the copy says so.
 */
export function disconnectFromServer(
  storage: Storage = window.localStorage,
  clearToken: () => void = () => {},
  reload: (url: string) => void = (url) => window.location.assign(url),
  sessionStorage: Storage = window.sessionStorage,
): void {
  // In-memory first: the auth provider holds signals that outlive a storage
  // write, so clearing only the keys leaves a signed-in shell pointing at a
  // server it has no credential for.
  clearToken()
  for (const store of [storage, sessionStorage]) {
    for (const key of [
      SERVER_URL_KEY,
      TOKEN_KEY,
      OAUTH_SESSION_KEY,
      OAUTH_PENDING_KEY,
    ]) {
      try {
        store.removeItem(key)
      } catch {
        // A store that refuses writes has nothing to forget.
      }
    }
  }
  // A full document load rather than in-app navigation: the base URL is baked
  // into the service graph at construction (`createServices`), so nothing
  // short of rebuilding it can point the app somewhere else. `/` rather than
  // the current path, because the current path belongs to the old server.
  reload('/')
}
