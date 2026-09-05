import { computed, signal, type ReadonlySignal } from '@preact/signals'
import { useState } from 'preact/hooks'
import { browserPlatform, type Platform } from '../platform'
import {
  createAuthPersistence,
  type AuthPersistence,
  type AuthStorageMode,
} from './persistence'
import type { AuthProvider, OAuthCallbackResult } from './provider'

const SESSION_KEY = 'axon.oauth.session'
const PENDING_KEY = 'axon.oauth.pending'
const REFRESH_SKEW_MS = 60_000
const DEFAULT_CLIENT_ID = 'axon-web'

/**
 * Least time between two refreshes provoked by a `401`. `getToken` already
 * refreshes ahead of expiry, so a `401` means something unexpected; if the
 * server were to answer `401` to everything while still honoring refreshes,
 * this bounds the retry rate to something harmless.
 */
const AUTH_FAILURE_REFRESH_COOLDOWN_MS = 5_000

/**
 * The server answered and refused the grant — it is bad, expired, or revoked.
 * The only error that may end a session.
 */
export class OAuthRejectedError extends Error {
  readonly name = 'OAuthRejectedError'
}

/**
 * We never got a verdict: the request failed, or the server failed to serve it.
 * Says nothing about the credentials, so it must never discard them.
 *
 * This distinction is the whole point. Collapsing it — treating any thrown
 * error as a dead session — meant a deploy could sign every client out: the
 * restart drops the live socket, the reconnect calls `getToken`, a token stale
 * for longer than the hour it lives triggers a refresh, and that refresh lands
 * on the same process that is still restarting. A 30-day refresh token was then
 * thrown away over a connection refused.
 */
export class OAuthTransportError extends Error {
  readonly name = 'OAuthTransportError'
}

/**
 * The one OAuth error code that means *this refresh token is no good* (RFC 6749
 * §5.2: bad, expired, revoked, or issued to another client).
 *
 * Classification is by error code and not by status class, because 4xx is far
 * too broad to mean "credential refused". `/v1/oauth/token` sits behind the
 * OAuth rate limiter (`crates/axon-api/src/oauth/rate_limit.rs`, 30/min per IP),
 * and a deploy is exactly when that trips: every tab's socket drops at once and
 * every reconnect asks for a token. Reading that 429 as a refusal would discard
 * a valid 30-day refresh token — the same deploy-time sign-out this whole split
 * exists to prevent, reintroduced through a narrower door.
 */
const GRANT_REJECTED = 'invalid_grant'

/** The OAuth error code from a token-endpoint error body, if it has one. */
function oauthErrorCode(body: unknown): string | null {
  if (typeof body !== 'object' || body === null) {
    return null
  }
  const { error } = body as { error?: unknown }
  return typeof error === 'string' ? error : null
}

/**
 * A human-readable message from a token-endpoint error body. Handles both
 * shapes this endpoint can produce: the OAuth one (`{error, error_description}`)
 * from the handler, and Axon's own envelope (`{error: {code, message}}`) from a
 * layer in front of it, such as the rate limiter. Falling through to
 * `String(error)` on the latter is what previously rendered `[object Object]`.
 */
function tokenErrorMessage(body: unknown, status: number): string {
  if (typeof body === 'object' && body !== null) {
    const record = body as Record<string, unknown>
    if (typeof record.error_description === 'string') {
      return record.error_description
    }
    if (typeof record.error === 'string') {
      return record.error
    }
    const nested = record.error
    if (typeof nested === 'object' && nested !== null) {
      const message = (nested as Record<string, unknown>).message
      if (typeof message === 'string') {
        return message
      }
    }
  }
  return `OAuth token exchange failed (HTTP ${status})`
}

export interface OAuthProviderConfig {
  provider: string
  label: string
}

interface OAuthSession {
  accessToken: string
  refreshToken: string
  expiresAt: number
  provider: string
}

interface PendingOAuth {
  state: string
  codeVerifier: string
  provider: string
  redirectUri: string
  createdAt: number
  storageMode: AuthStorageMode
}

interface TokenSuccessBody {
  access_token: string
  token_type: string
  expires_in: number
  refresh_token: string
}

export interface OAuthAuthProvider extends AuthProvider {
  signedIn: ReadonlySignal<boolean>
  providers: readonly OAuthProviderConfig[]
  clearToken(): void
  startSignIn(provider: string): Promise<void>
  completeRedirect(url: URL): Promise<OAuthCallbackResult>
}

export interface OAuthAuthOptions {
  providers: readonly OAuthProviderConfig[]
  baseUrl: string
  clientId?: string
  /**
   * Origin the OAuth callback (`/oauth/callback`) is resolved against.
   * Defaults to `window.location.origin`. A future Tauri build (M-W12) would
   * pass a deep-link scheme here instead of changing `startSignIn`.
   */
  redirectUriBase?: string
  persistence?: AuthPersistence
  storage?: Storage
  sessionStorage?: Storage
  rememberMeDefault?: boolean
  pendingStorage?: Storage
  navigate?: (url: string) => void
  /**
   * Transport seam (ADR 0102 § 2) for the token exchange. This one call is not
   * an `openapi-fetch` operation — the token endpoint is form-encoded per
   * RFC 6749, not the `/v1` JSON envelope — so it takes the platform directly.
   */
  platform?: Pick<Platform, 'fetch'>
}

export function parseOAuthProviders(
  raw: string | undefined,
): OAuthProviderConfig[] {
  if (raw === undefined || raw.trim() === '') {
    return []
  }
  return raw
    .split(',')
    .map((entry) => {
      const [provider, ...labelParts] = entry.split(':')
      const trimmedProvider = provider.trim()
      const label = labelParts.join(':').trim() || trimmedProvider
      return { provider: trimmedProvider, label }
    })
    .filter(({ provider }) => /^[a-z][a-z0-9_-]*$/.test(provider))
}

export function createOAuthAuthProvider({
  providers,
  baseUrl,
  clientId = DEFAULT_CLIENT_ID,
  redirectUriBase = window.location.origin,
  persistence: providedPersistence,
  storage,
  sessionStorage,
  rememberMeDefault,
  pendingStorage = window.sessionStorage,
  navigate = (url) => window.location.assign(url),
  platform = browserPlatform(),
}: OAuthAuthOptions): OAuthAuthProvider {
  const fetch = platform.fetch
  const persistence =
    providedPersistence ??
    createAuthPersistence({
      persistentStorage: storage,
      sessionStorage,
      rememberMeDefault,
    })
  const storedSession = persistence.read(SESSION_KEY)
  const session = signal<OAuthSession | null>(
    parseSession(storedSession?.value ?? null),
  )
  const sessionMode = signal<AuthStorageMode | null>(
    session.value === null ? null : (storedSession?.mode ?? 'persistent'),
  )
  let refreshInFlight: Promise<string | null> | null = null
  let lastAuthFailureRefreshAt = 0

  function persist(next: OAuthSession | null, mode = sessionMode.value): void {
    if (next === null) {
      persistence.remove(SESSION_KEY)
      sessionMode.value = null
    } else {
      const target =
        mode ?? (persistence.rememberMe.value ? 'persistent' : 'session')
      persistence.write(SESSION_KEY, JSON.stringify(next), target)
      sessionMode.value = target
    }
    session.value = next
  }

  async function redeem(form: URLSearchParams): Promise<OAuthSession> {
    let response: Response
    try {
      response = await fetch(apiUrl('/v1/oauth/token', baseUrl), {
        method: 'POST',
        headers: { 'content-type': 'application/x-www-form-urlencoded' },
        body: form,
      })
    } catch (cause) {
      throw new OAuthTransportError('could not reach the server', { cause })
    }
    const body = (await response.json().catch(() => ({}))) as unknown
    if (!response.ok) {
      const message = tokenErrorMessage(body, response.status)
      // Only an explicit `invalid_grant` ends a session. Everything else — a
      // 429 from the rate limiter, a 5xx, a proxy mid-restart, an error body we
      // cannot read — is "no verdict", and the credentials stay.
      throw oauthErrorCode(body) === GRANT_REJECTED
        ? new OAuthRejectedError(message)
        : new OAuthTransportError(message)
    }
    return sessionFromTokenBody(
      body,
      form.get('provider') ?? session.value?.provider ?? '',
    )
  }

  async function refreshAccessToken(): Promise<string | null> {
    if (refreshInFlight !== null) {
      return refreshInFlight
    }
    refreshInFlight = (async () => {
      const current = session.value
      if (current === null) {
        return null
      }
      try {
        const next = await redeem(
          new URLSearchParams({
            grant_type: 'refresh_token',
            refresh_token: current.refreshToken,
          }),
        )
        persist({ ...next, provider: current.provider })
        return next.accessToken
      } catch (error) {
        // Only a refusal ends the session. Not knowing is not a reason to throw
        // away a refresh token that is good for thirty days — the caller gets
        // `null` and no token, the request it wanted fails, and the next
        // attempt (a retry, a reconnect, the user acting again) tries afresh.
        if (error instanceof OAuthRejectedError) {
          persist(null)
        }
        return null
      } finally {
        refreshInFlight = null
      }
    })()
    return refreshInFlight
  }

  const provider: OAuthAuthProvider = {
    providers,
    getToken() {
      const current = session.value
      if (current === null) {
        return null
      }
      if (current.expiresAt - Date.now() > REFRESH_SKEW_MS) {
        return current.accessToken
      }
      return refreshAccessToken()
    },
    onAuthFailure() {
      // A `401` says this access token was not accepted; it does not say the
      // session is over, and we hold a refresh token that outlives the access
      // token thirty times over. So try to refresh — `refreshAccessToken` signs
      // out if and only if the server refuses the grant. Signing out here
      // instead (the previous behavior) turned every unexpected `401` into a
      // full re-authentication, including ones a refresh would have fixed.
      //
      // Fire-and-forget: the interface is synchronous and the request that
      // failed is already lost. What this buys is the *next* request.
      if (session.value === null) {
        return
      }
      const now = Date.now()
      if (now - lastAuthFailureRefreshAt < AUTH_FAILURE_REFRESH_COOLDOWN_MS) {
        return
      }
      lastAuthFailureRefreshAt = now
      void refreshAccessToken()
    },
    clearToken() {
      persist(null)
      pendingStorage.removeItem(PENDING_KEY)
    },
    signedIn: computed(() => session.value !== null),
    async startSignIn(providerName: string) {
      if (!providers.some(({ provider }) => provider === providerName)) {
        throw new Error('unknown OAuth provider')
      }
      const codeVerifier = randomBase64Url(32)
      const codeChallenge = await sha256Base64Url(codeVerifier)
      const state = randomBase64Url(32)
      const redirectUri = new URL('/oauth/callback', redirectUriBase).toString()
      const storageMode = persistence.rememberMe.value
        ? 'persistent'
        : 'session'
      const pending: PendingOAuth = {
        state,
        codeVerifier,
        provider: providerName,
        redirectUri,
        createdAt: Date.now(),
        storageMode,
      }
      pendingStorage.setItem(PENDING_KEY, JSON.stringify(pending))

      const authorize = new URL(apiUrl('/v1/oauth/authorize', baseUrl))
      authorize.searchParams.set('response_type', 'code')
      authorize.searchParams.set('client_id', clientId)
      authorize.searchParams.set('redirect_uri', redirectUri)
      authorize.searchParams.set('code_challenge', codeChallenge)
      authorize.searchParams.set('code_challenge_method', 'S256')
      authorize.searchParams.set('provider', providerName)
      authorize.searchParams.set('state', state)
      navigate(authorize.toString())
    },
    async completeRedirect(url: URL): Promise<OAuthCallbackResult> {
      const error = url.searchParams.get('error')
      if (error !== null) {
        return {
          ok: false,
          message:
            url.searchParams.get('error_description') ??
            `OAuth provider rejected sign-in: ${error}`,
        }
      }
      const code = url.searchParams.get('code')
      const state = url.searchParams.get('state')
      if (code === null || state === null) {
        return { ok: false, message: 'OAuth callback is missing code or state' }
      }

      const pending = parsePending(pendingStorage.getItem(PENDING_KEY))
      if (pending === null || pending.state !== state) {
        pendingStorage.removeItem(PENDING_KEY)
        return { ok: false, message: 'OAuth sign-in state did not match' }
      }

      try {
        const next = await redeem(
          new URLSearchParams({
            grant_type: 'authorization_code',
            code,
            code_verifier: pending.codeVerifier,
            client_id: clientId,
            redirect_uri: pending.redirectUri,
            provider: pending.provider,
          }),
        )
        persist({ ...next, provider: pending.provider }, pending.storageMode)
        pendingStorage.removeItem(PENDING_KEY)
        window.history.replaceState(null, '', '/')
        return { ok: true }
      } catch (err) {
        return {
          ok: false,
          message: err instanceof Error ? err.message : 'OAuth sign-in failed',
        }
      }
    },
    LoginBootstrap: () => <OAuthButtons provider={provider} />,
  }
  return provider
}

function OAuthButtons({ provider }: { provider: OAuthAuthProvider }) {
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  if (provider.providers.length === 0) {
    return null
  }

  return (
    <div class="sso-options">
      {provider.providers.map(({ provider: id, label }) => {
        const brand = providerBrand(id, label)
        return (
          <button
            key={id}
            type="button"
            class={`sso-button ${brand.className}`}
            disabled={busy !== null}
            onClick={() => {
              setBusy(id)
              setError(null)
              void provider.startSignIn(id).catch((err) => {
                setBusy(null)
                setError(err instanceof Error ? err.message : 'Sign-in failed')
              })
            }}
          >
            <span class="sso-icon" aria-hidden="true">
              {brand.icon}
            </span>
            <span>{busy === id ? 'Opening...' : brand.text}</span>
          </button>
        )
      })}
      {error !== null && <p class="error">{error}</p>}
    </div>
  )
}

function providerBrand(provider: string, label: string) {
  switch (provider) {
    case 'google':
      return {
        className: 'sso-google',
        text: 'Continue with Google',
        icon: <GoogleIcon />,
      }
    case 'microsoft':
      return {
        className: 'sso-microsoft',
        text: 'Sign in with Microsoft',
        icon: <MicrosoftIcon />,
      }
    case 'apple':
      return {
        className: 'sso-apple',
        text: 'Sign in with Apple',
        icon: <AppleIcon />,
      }
    default:
      return {
        className: 'sso-generic',
        text: `Continue with ${label}`,
        icon: <GenericProviderIcon />,
      }
  }
}

function GoogleIcon() {
  return (
    <svg viewBox="0 0 18 18" role="img" aria-label="Google">
      <path
        fill="#4285f4"
        d="M17.64 9.2c0-.64-.06-1.25-.16-1.84H9v3.48h4.84a4.14 4.14 0 0 1-1.8 2.72v2.26h2.91c1.7-1.57 2.69-3.88 2.69-6.62z"
      />
      <path
        fill="#34a853"
        d="M9 18c2.43 0 4.47-.8 5.96-2.18l-2.91-2.26c-.81.54-1.84.86-3.05.86a5.38 5.38 0 0 1-5.06-3.72H.93v2.33A9 9 0 0 0 9 18z"
      />
      <path
        fill="#fbbc05"
        d="M3.94 10.7A5.4 5.4 0 0 1 3.66 9c0-.59.1-1.16.28-1.7V4.97H.93A9 9 0 0 0 0 9c0 1.45.34 2.82.93 4.03l3.01-2.33z"
      />
      <path
        fill="#ea4335"
        d="M9 3.58c1.32 0 2.51.45 3.44 1.35l2.58-2.58A8.65 8.65 0 0 0 9 0 9 9 0 0 0 .93 4.97L3.94 7.3A5.38 5.38 0 0 1 9 3.58z"
      />
    </svg>
  )
}

function MicrosoftIcon() {
  return (
    <svg viewBox="0 0 23 23" role="img" aria-label="Microsoft">
      <path fill="#f25022" d="M1 1h10v10H1z" />
      <path fill="#7fba00" d="M12 1h10v10H12z" />
      <path fill="#00a4ef" d="M1 12h10v10H1z" />
      <path fill="#ffb900" d="M12 12h10v10H12z" />
    </svg>
  )
}

function AppleIcon() {
  return (
    <svg viewBox="0 0 24 24" role="img" aria-label="Apple">
      <path
        fill="currentColor"
        d="M16.37 1.43c0 1.08-.4 2.03-1.18 2.86-.86.91-1.9 1.44-3.02 1.36-.13-1.04.43-2.13 1.18-2.94.82-.89 2.23-1.56 3.02-1.28zM20.3 17.48c-.5 1.17-.74 1.69-1.38 2.72-.9 1.46-2.17 3.28-3.75 3.3-1.4.01-1.76-.95-3.67-.94-1.9 0-2.3.96-3.69.95-1.58-.02-2.78-1.66-3.69-3.12-2.52-4.08-2.79-8.87-1.23-11.42 1.11-1.82 2.86-2.88 4.5-2.88 1.67 0 2.73.97 4.11.97 1.34 0 2.15-.97 4.08-.97 1.45 0 3 .82 4.1 2.24-3.6 2.08-3.02 7.44.62 9.15z"
      />
    </svg>
  )
}

function GenericProviderIcon() {
  return (
    <svg viewBox="0 0 24 24" role="img" aria-label="Identity provider">
      <path
        fill="currentColor"
        d="M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8zm0 2c-4.42 0-8 2.24-8 5v1h16v-1c0-2.76-3.58-5-8-5z"
      />
    </svg>
  )
}

function parseSession(raw: string | null): OAuthSession | null {
  if (raw === null) {
    return null
  }
  try {
    const value = JSON.parse(raw) as Partial<OAuthSession>
    if (
      typeof value.accessToken === 'string' &&
      typeof value.refreshToken === 'string' &&
      typeof value.expiresAt === 'number' &&
      typeof value.provider === 'string'
    ) {
      return {
        accessToken: value.accessToken,
        refreshToken: value.refreshToken,
        expiresAt: value.expiresAt,
        provider: value.provider,
      }
    }
  } catch {
    // Corrupt auth state should sign out, not wedge the app.
  }
  return null
}

function parsePending(raw: string | null): PendingOAuth | null {
  if (raw === null) {
    return null
  }
  try {
    const value = JSON.parse(raw) as Partial<PendingOAuth>
    if (
      typeof value.state === 'string' &&
      typeof value.codeVerifier === 'string' &&
      typeof value.provider === 'string' &&
      typeof value.redirectUri === 'string' &&
      typeof value.createdAt === 'number'
    ) {
      return {
        state: value.state,
        codeVerifier: value.codeVerifier,
        provider: value.provider,
        redirectUri: value.redirectUri,
        createdAt: value.createdAt,
        storageMode: value.storageMode === 'session' ? 'session' : 'persistent',
      }
    }
  } catch {
    // Ignore corrupt pending state.
  }
  return null
}

function sessionFromTokenBody(body: unknown, provider: string): OAuthSession {
  const token = body as Partial<TokenSuccessBody>
  if (
    typeof token.access_token !== 'string' ||
    typeof token.refresh_token !== 'string' ||
    typeof token.expires_in !== 'number' ||
    typeof token.token_type !== 'string' ||
    token.token_type.toLowerCase() !== 'bearer'
  ) {
    throw new Error('OAuth token response was not usable')
  }
  return {
    accessToken: token.access_token,
    refreshToken: token.refresh_token,
    expiresAt: Date.now() + token.expires_in * 1000,
    provider,
  }
}

function apiUrl(path: string, baseUrl: string): string {
  return new URL(path, new URL(baseUrl, window.location.origin)).toString()
}

function randomBase64Url(bytes: number): string {
  const data = new Uint8Array(bytes)
  crypto.getRandomValues(data)
  return base64Url(data)
}

async function sha256Base64Url(value: string): Promise<string> {
  const bytes = new TextEncoder().encode(value)
  const digest = await crypto.subtle.digest('SHA-256', bytes)
  return base64Url(new Uint8Array(digest))
}

function base64Url(bytes: Uint8Array): string {
  let binary = ''
  for (const byte of bytes) {
    binary += String.fromCharCode(byte)
  }
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}
