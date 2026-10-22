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
   * `clearToken` waits for the hook. A rejection leaves the session in place.
   */
  onBeforeSignOut(
    hook: (token: string | null) => void | Promise<void>,
  ): () => void
  /**
   * Drop the local session without running sign-out hooks. A push delete that
   * succeeds on a later attempt uses this so it does not DELETE again.
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
      return Promise.all(waits).then(
        () => {
          provider.finishSignOut()
          return true
        },
        () => false,
      )
    },
    finishSignOut() {
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
