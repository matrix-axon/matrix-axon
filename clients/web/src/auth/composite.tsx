import { computed } from '@preact/signals'
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
   * A hook must not throw out of `clearToken`.
   */
  onBeforeSignOut(
    hook: (token: string | null) => void | Promise<void>,
  ): () => void
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

  function active(): AuthProvider {
    return oauth.signedIn.value ? oauth : tokenPaste
  }

  const provider: CompositeAuthProvider = {
    oauth,
    tokenPaste,
    getToken: () => active().getToken(),
    onAuthFailure() {
      active().onAuthFailure()
    },
    clearToken() {
      const token = provider.peekToken()
      for (const hook of [...signOutHooks]) {
        try {
          const result = hook(token)
          if (result instanceof Promise) {
            void result.catch(() => {})
          }
        } catch {
          // Sign-out still clears the session when a hook fails.
        }
      }
      oauth.clearToken()
      tokenPaste.clearToken()
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
