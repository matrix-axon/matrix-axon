import type { ReadonlySignal } from '@preact/signals'
import type { ComponentType } from 'preact'

/**
 * The auth seam (ADR 0046, M-W2; required by ADR 0031).
 *
 * The rest of the client never learns how a bearer token is obtained or
 * stored: the API layer pulls the current token from here and reports auth
 * failures back, and the app shell renders `LoginBootstrap` whenever there is
 * no usable token. Token-paste over `localStorage` is the alpha
 * implementation; OAuth 2.0 + PKCE and a Tauri OS-keychain provider drop in
 * later without touching consumers.
 */
export interface AuthProvider {
  /** Opaque reader identity. Changes on sign-in/sign-out, never token renewal. */
  sessionId?: ReadonlySignal<string>
  /** Current credential without refresh or I/O; used to classify late 401s. */
  peekToken?(): string | null
  /**
   * The bearer token to authenticate the next request with, or `null` when
   * signed out. May be async so providers that refresh (OAuth) fit the same
   * seam; callers `await` the result either way.
   */
  getToken(): string | null | Promise<string | null>

  /**
   * Called by the API layer whenever the server rejects the credential
   * (HTTP 401): the token is missing, revoked, or expired. The provider
   * decides what that means — token-paste discards the stored token so the
   * app falls back to `LoginBootstrap`; an OAuth provider would start a
   * refresh or re-auth flow.
   */
  onAuthFailure(): void

  /**
   * UI slot the app shell renders when no token is available — the
   * provider-specific way for a human to establish one (a paste form, an
   * OAuth redirect button, …).
   */
  LoginBootstrap: ComponentType
}

export type OAuthCallbackResult = { ok: true } | { ok: false; message: string }

/**
 * The app-shell auth surface. Transport code only needs `AuthProvider`; the
 * shell also needs a reactive signed-in bit, explicit sign-out, and the
 * browser redirect callback used by OAuth Path A.
 */
export interface AppAuthProvider extends AuthProvider {
  signedIn: ReadonlySignal<boolean>
  /**
   * Drop the local session. May return a promise when a hook has to finish
   * first (the push registration delete). `false` means the session was kept.
   */
  clearToken(): void | Promise<boolean | void>
  completeOAuthRedirect(url: URL): Promise<OAuthCallbackResult>
}
