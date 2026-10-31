import { batch, computed, type ReadonlySignal } from '@preact/signals'
import { useId } from 'preact/hooks'
import type {
  AppAuthProvider,
  AuthProvider,
  OAuthCallbackResult,
} from './provider'
import {
  createOAuthAuthProvider,
  type OAuthAuthOptions,
  type OAuthAuthProvider,
} from './oauth'
import { createAuthPersistence, type AuthPersistence } from './persistence'
import {
  createTokenPasteProvider,
  type TokenPasteProvider,
} from './token-paste'

export interface CompositeAuthProvider extends AppAuthProvider {
  sessionId: ReadonlySignal<string>
  oauth: OAuthAuthProvider
  tokenPaste: TokenPasteProvider
  /**
   * Refresh the OAuth session even when its access token is still fresh.
   * A pasted or management token has no refresh grant and answers
   * `'unsupported'`.
   */
  refreshNow(): Promise<'refreshed' | 'unsupported' | 'failed'>
  /** The bearer currently held, before {@link clearToken} drops it. */
  peekToken(): string | null
  /**
   * Runs before the session is cleared. The argument is {@link peekToken}.
   * `clearToken` waits for the hook. A rejection leaves the session in place.
   */
  onBeforeSignOut(
    hook: (token: string | null) => void | Promise<void>,
  ): () => void
  /** Runs after every sign-out barrier settles if the session was retained. */
  onSignOutAborted(hook: () => void | Promise<void>): () => void
  /**
   * Internal session-drop primitive after all barriers succeed. Application
   * integrations, including deferred push deletion, must use `clearToken`.
   */
  finishSignOut(): void
  /** See {@link OAuthAuthProvider.onAccessTokenRefreshed}. */
  onAccessTokenRefreshed(listener: () => void): () => void
}

export function createCompositeAuthProvider(
  options: OAuthAuthOptions,
): CompositeAuthProvider {
  const persistence =
    options.persistence ??
    createAuthPersistence({
      persistentStorage: options.storage,
      sessionStorage: options.sessionStorage,
      rememberMeDefault: options.rememberMeDefault,
    })
  const oauth = createOAuthAuthProvider({ ...options, persistence })
  const tokenPaste = createTokenPasteProvider(persistence)
  const signOutHooks = new Set<(token: string | null) => void | Promise<void>>()
  const abortHooks = new Set<() => void | Promise<void>>()
  let signingOut: Promise<boolean> | null = null

  function active(): AuthProvider {
    return oauth.signedIn.value ? oauth : tokenPaste
  }

  const provider: CompositeAuthProvider = {
    sessionId: computed(() =>
      oauth.signedIn.value
        ? `oauth:${oauth.sessionId.value}`
        : `token:${tokenPaste.sessionId.value}`,
    ),
    oauth,
    tokenPaste,
    getToken: () => active().getToken(),
    onAuthFailure() {
      active().onAuthFailure()
    },
    clearToken() {
      if (signingOut !== null) return signingOut
      const token = provider.peekToken()
      const identity = provider.sessionId.peek()
      const waits: Promise<void>[] = []
      for (const hook of [...signOutHooks]) {
        try {
          const result = hook(token)
          if (result instanceof Promise) {
            waits.push(result)
          }
        } catch (error) {
          waits.push(Promise.reject(error))
        }
      }
      if (waits.length === 0) {
        provider.finishSignOut()
        return
      }
      // A failed barrier must not restore authority while another barrier is
      // still revoking it. Concurrent attempts share this completion too.
      signingOut = Promise.allSettled(waits)
        .then(async (results) => {
          if (
            results.every((result) => result.status === 'fulfilled') &&
            provider.sessionId.peek() === identity
          ) {
            provider.finishSignOut()
            return true
          }
          await Promise.allSettled([...abortHooks].map(async (hook) => hook()))
          return false
        })
        .finally(() => {
          signingOut = null
        })
      return signingOut
    },
    finishSignOut() {
      // Publish one signed-out state, never the hidden pasted credential
      // between clearing the preferred OAuth session and its fallback.
      batch(() => {
        oauth.clearToken()
        tokenPaste.clearToken()
      })
    },
    refreshNow() {
      if (!oauth.signedIn.value) {
        return Promise.resolve('unsupported')
      }
      return oauth.refreshNow()
    },
    peekToken() {
      if (oauth.signedIn.value) {
        return oauth.peekToken()
      }
      const pasted = tokenPaste.getToken()
      return typeof pasted === 'string' ? pasted : null
    },
    onBeforeSignOut(hook) {
      signOutHooks.add(hook)
      return () => {
        signOutHooks.delete(hook)
      }
    },
    onSignOutAborted(hook) {
      abortHooks.add(hook)
      return () => {
        abortHooks.delete(hook)
      }
    },
    onAccessTokenRefreshed(listener) {
      return oauth.onAccessTokenRefreshed(listener)
    },
    signedIn: computed(() => oauth.signedIn.value || tokenPaste.signedIn.value),
    completeOAuthRedirect(url: URL): Promise<OAuthCallbackResult> {
      return oauth.completeRedirect(url)
    },
    LoginBootstrap: () => (
      <div class="signin-options">
        <RememberMe persistence={persistence} />
        <oauth.LoginBootstrap />
        {oauth.providers.value.length > 0 && (
          <div class="signin-divider" aria-hidden="true">
            or
          </div>
        )}
        <tokenPaste.LoginBootstrap />
      </div>
    ),
  }
  return provider
}

function RememberMe({ persistence }: { persistence: AuthPersistence }) {
  const id = useId()
  return (
    <label class="remember-me" for={id}>
      <input
        id={id}
        type="checkbox"
        checked={persistence.rememberMe.value}
        onInput={(event) => {
          persistence.rememberMe.value = event.currentTarget.checked
        }}
      />
      Remember me
    </label>
  )
}
