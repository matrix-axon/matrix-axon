import { computed, signal, type ReadonlySignal } from '@preact/signals'
import { useState } from 'preact/hooks'
import {
  createAuthPersistence,
  type AuthPersistence,
  type AuthPersistenceOptions,
} from './persistence'
import type { AuthProvider } from './provider'

/**
 * `localStorage` key for the pasted token. The exposure model is the
 * GitHub-PAT one (ADR 0046): the token is self-minted (`axon token issue`)
 * and revocable (`axon token revoke`), and OAuth replaces this flow post-MVP.
 */
export const STORAGE_KEY = 'axon.token'

/** The token-paste [`AuthProvider`], with the mutations the paste UI needs. */
export interface TokenPasteProvider extends AuthProvider {
  /** Store a pasted token (trimmed) and switch to the signed-in state. */
  setToken(token: string): void
  /** Discard the stored token — explicit sign-out. */
  clearToken(): void
  /** Reactive signed-in flag for the app shell to branch on. */
  signedIn: ReadonlySignal<boolean>
}

/**
 * The alpha auth implementation (ADR 0046, M-W2): the user pastes a token
 * minted with `axon token issue`; it lives in `localStorage` behind this seam.
 * A 401 from the server discards it, which flips `signedIn` and puts the app
 * back on the paste form.
 */
export function createTokenPasteProvider(
  options:
    | AuthPersistence
    | AuthPersistenceOptions
    | Storage = createAuthPersistence(),
): TokenPasteProvider {
  const persistence =
    'read' in options
      ? options
      : 'getItem' in options
        ? createAuthPersistence({ persistentStorage: options })
        : createAuthPersistence(options)
  const token = signal<string | null>(
    persistence.read(STORAGE_KEY)?.value ?? null,
  )

  const discard = () => {
    persistence.remove(STORAGE_KEY)
    token.value = null
  }

  const provider: TokenPasteProvider = {
    getToken: () => token.value,
    onAuthFailure: discard,
    clearToken: discard,
    setToken(value: string) {
      const trimmed = value.trim()
      if (trimmed === '') {
        return
      }
      persistence.write(STORAGE_KEY, trimmed)
      token.value = trimmed
    },
    signedIn: computed(() => token.value !== null),
    LoginBootstrap: () => <TokenPasteForm provider={provider} />,
  }
  return provider
}

function TokenPasteForm({ provider }: { provider: TokenPasteProvider }) {
  const [draft, setDraft] = useState('')

  return (
    <form
      onSubmit={(event) => {
        event.preventDefault()
        provider.setToken(draft)
      }}
    >
      <label>
        Access token
        <input
          type="password"
          value={draft}
          placeholder="paste a token from `axon token issue`"
          onInput={(event) => setDraft(event.currentTarget.value)}
        />
      </label>
      <button type="submit" disabled={draft.trim() === ''}>
        Sign in
      </button>
    </form>
  )
}
