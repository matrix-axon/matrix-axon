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
      oauth.clearToken()
      tokenPaste.clearToken()
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
