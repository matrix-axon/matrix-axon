import { useCallback, useEffect, useRef, useState } from 'preact/hooks'
import { apiErrorCode, apiErrorMessage } from '../api/client'
import type { components } from '../api/schema'
import { OAuthStepUpRequiredError } from '../auth/oauth'
import { NativeSignInCancelled } from '../platform'
import { useServices } from '../services'

type Identity = components['schemas']['OauthIdentityDto']

/**
 * A credential change that was refused with `recent_sign_in_required` and is
 * waiting for the sign-in it asked for (ADR 0109).
 *
 * In `sessionStorage` because a browser sign-in replaces this page: the
 * redirect comes back as a fresh load, and without this the user would sign in
 * again only to be asked to repeat what they had already confirmed.
 */
export const RESUME_KEY = 'axon.management.resume'

/**
 * How long an intent stays good. The server's own window for "recent" is ten
 * minutes, so anything older could not succeed anyway; more to the point, a
 * confirmation that old is no longer something the user is in the middle of.
 */
const RESUME_TTL_MS = 10 * 60_000

type Change =
  | {
      kind: 'unlink'
      identityId: string
      allowLockout: boolean
      /** Whether this session signed in with it, so unlinking ends it. */
      current: boolean
    }
  | { kind: 'link-apple' }

type Intent = Change & { createdAt: number }

function readIntent(): Intent | null {
  try {
    const raw = window.sessionStorage.getItem(RESUME_KEY)
    if (raw === null) {
      return null
    }
    const value = JSON.parse(raw) as Partial<Intent>
    if (
      typeof value.createdAt !== 'number' ||
      Date.now() - value.createdAt > RESUME_TTL_MS
    ) {
      clearIntent()
      return null
    }
    if (value.kind === 'link-apple') {
      return { kind: 'link-apple', createdAt: value.createdAt }
    }
    if (value.kind === 'unlink' && typeof value.identityId === 'string') {
      return {
        kind: 'unlink',
        identityId: value.identityId,
        allowLockout: value.allowLockout === true,
        current: value.current === true,
        createdAt: value.createdAt,
      }
    }
  } catch {
    // Unreadable storage or a corrupt entry: there is nothing to resume.
  }
  return null
}

function writeIntent(change: Change): void {
  try {
    window.sessionStorage.setItem(
      RESUME_KEY,
      JSON.stringify({ ...change, createdAt: Date.now() }),
    )
  } catch {
    // Without storage a browser redirect cannot resume; the user repeats the
    // action after signing in, which then passes.
  }
}

function clearIntent(): void {
  try {
    window.sessionStorage.removeItem(RESUME_KEY)
  } catch {
    // Nothing to clear.
  }
}

function providerName(provider: string): string {
  return provider.charAt(0).toUpperCase() + provider.slice(1)
}

/**
 * Where the user removes Axon from the provider's own list of apps. Unlinking
 * cannot do that for them (ADR 0109): Axon forgets the identity, and the
 * provider goes on listing Axon until the user removes it there.
 */
function providerControls(provider: string) {
  switch (provider) {
    case 'apple':
      return (
        <>
          To remove it there too, open Settings on your iPhone, iPad or Mac,
          choose your name, then Sign in with Apple.
        </>
      )
    case 'google':
      return (
        <>
          To remove it there too, open your Google Account's third-party
          connections (myaccount.google.com/connections).
        </>
      )
    case 'microsoft':
      return (
        <>
          To remove it there too, open your Microsoft account's privacy settings
          and look under the apps and services you have given access to.
        </>
      )
    default:
      return <>To remove it there too, use that account's own settings.</>
  }
}

/**
 * The sign-in identities linked to this server's owner, with Unlink, and the
 * place an Apple ID is linked (ADR 0109 step 3, `/v1/management/oauth/*`).
 *
 * Renders nothing unless there is something to show or do. Where the operator
 * has switched the management API off (the list answers `403
 * management_disabled`), the list and Unlink are hidden, and what remains is
 * the link action, which is an OAuth route and not a management one.
 */
export function LinkedSignIns() {
  const { api, auth } = useServices()
  const oauth = auth.oauth
  // `null` until the server has said whether it serves the management API.
  const [management, setManagement] = useState<boolean | null>(null)
  const [identities, setIdentities] = useState<Identity[] | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [confirming, setConfirming] = useState<{
    identityId: string
    lastCredential: boolean
  } | null>(null)
  // The identity id being unlinked, or `'link'`.
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [handedOff, setHandedOff] = useState(false)
  const mounted = useRef(true)
  useEffect(
    () => () => {
      mounted.current = false
    },
    [],
  )

  const load = useCallback(async () => {
    try {
      const {
        data,
        error: apiError,
        response,
      } = await api.GET('/v1/management/oauth/identities')
      if (!mounted.current) {
        return
      }
      if (apiError !== undefined) {
        // Switched off by the operator, or a server older than the route.
        if (
          apiErrorCode(apiError) === 'management_disabled' ||
          response.status === 404
        ) {
          setManagement(false)
          return
        }
        setManagement(true)
        setLoadError(apiErrorMessage(apiError))
        return
      }
      setManagement(true)
      setLoadError(null)
      setIdentities(data.data)
    } catch (cause) {
      if (mounted.current) {
        setManagement(true)
        setLoadError(cause instanceof Error ? cause.message : String(cause))
      }
    }
  }, [api])

  // The signed-out screen is the usual place providers are discovered;
  // Settings is only reached signed in, so it asks for itself. Shared and
  // idempotent.
  useEffect(() => {
    void oauth.discoverProviders()
  }, [oauth])

  // The list request is also how this learns whether management is served.
  // `GET /v1/status` states it outright, but it also computes backfill
  // progress across every stored event, which on a large instance runs for
  // minutes and holds a database connection the whole time. A handful of
  // Settings visits exhausted the server's pool that way (#614). This costs a
  // disabled server one cheap 403 instead.
  useEffect(() => {
    void load()
  }, [load])

  // Ask again when the app comes back to the front. A sign-in unlinked from
  // another device would otherwise stay listed here until Settings was
  // reopened. `management` gates it so a server without the list is not
  // asked on every return.
  useEffect(() => {
    if (management !== true) {
      return
    }
    const refresh = () => {
      if (document.visibilityState === 'visible') {
        void load()
      }
    }
    document.addEventListener('visibilitychange', refresh)
    window.addEventListener('focus', refresh)
    return () => {
      document.removeEventListener('visibilitychange', refresh)
      window.removeEventListener('focus', refresh)
    }
  }, [management, load])

  /** Make the change once. `'step-up'` means the server wants a fresh sign-in. */
  async function perform(change: Change): Promise<'done' | 'step-up'> {
    if (change.kind === 'link-apple') {
      const token = await auth.getToken()
      if (token === null) {
        throw new Error('Sign in first, then link your Apple ID.')
      }
      try {
        await oauth.bindApple(token)
      } catch (err) {
        if (err instanceof OAuthStepUpRequiredError) {
          return 'step-up'
        }
        throw err
      }
      await load()
      return 'done'
    }

    const { error: apiError, response } = await api.DELETE(
      '/v1/management/oauth/identities/{identity_id}',
      {
        params: {
          path: { identity_id: change.identityId },
          query: change.allowLockout ? { allow_lockout: true } : {},
        },
      },
    )
    if (apiError === undefined) {
      setConfirming(null)
      if (change.current) {
        // The server has already ended this session with the identity. Drop
        // the dead tokens now instead of waiting to trip over a 401.
        auth.clearToken()
        return 'done'
      }
      await load()
      return 'done'
    }
    switch (apiErrorCode(apiError)) {
      case 'recent_sign_in_required':
        return 'step-up'
      case 'last_credential':
        // Nothing was changed. Ask again, saying what it would cost.
        setConfirming({ identityId: change.identityId, lastCredential: true })
        return 'done'
      case 'management_disabled':
        setManagement(false)
        return 'done'
      default:
        if (response.status === 404) {
          // Already gone, from another device or the CLI.
          setConfirming(null)
          await load()
          return 'done'
        }
        throw new Error(apiErrorMessage(apiError))
    }
  }

  /**
   * Make the change, signing in again first if the server asks for it.
   *
   * `resumed` marks the attempt that follows that sign-in. It never starts
   * another: a second refusal is reported, so a provider that reports no fresh
   * authentication cannot bounce the user between here and its sign-in page.
   */
  async function run(change: Change, resumed = false): Promise<void> {
    if ((await perform(change)) === 'done') {
      return
    }
    if (resumed) {
      throw new Error(
        'The server still needs a more recent sign-in for this change. Sign out, sign in again, and retry within ten minutes.',
      )
    }
    const provider = oauth.sessionProvider.value
    await oauth.discoverProviders()
    if (
      provider === null ||
      !oauth.providers.value.some((entry) => entry.provider === provider)
    ) {
      // A pasted token that expires, or a session whose provider has since
      // been switched off: there is no sign-in flow here to re-run.
      throw new Error(
        'This change needs a sign-in from the last ten minutes. Sign out, sign in again, and retry.',
      )
    }
    writeIntent(change)
    let outcome
    try {
      outcome = await oauth.startSignIn(provider, { returnTo: '/settings' })
    } catch (err) {
      clearIntent()
      throw err
    }
    if (outcome === 'signed-in') {
      // A native sheet, finished in place.
      clearIntent()
      await run(change, true)
      return
    }
    // In a browser the page is already being replaced. In a shell the sign-in
    // is in the user's browser, and the effect below picks it up on return.
    setHandedOff(true)
  }

  function start(change: Change, resumed = false): void {
    if (!resumed) {
      // Whatever was waiting on a sign-in is superseded by what the user has
      // just asked for. Left in place, it would be finished by the *next*
      // sign-in of any kind, including the one this action is about to make,
      // and an unlink confirmed minutes ago would go through unannounced.
      clearIntent()
    }
    setBusy(change.kind === 'unlink' ? change.identityId : 'link')
    setError(null)
    setHandedOff(false)
    void run(change, resumed)
      .catch((err: unknown) => {
        // Dismissing Apple's sheet is a choice, not an error.
        if (mounted.current && !(err instanceof NativeSignInCancelled)) {
          setError(err instanceof Error ? err.message : 'The change failed')
        }
      })
      .finally(() => {
        if (mounted.current) {
          setBusy(null)
        }
      })
  }

  // Finish a change that was waiting on a sign-in, once that sign-in has
  // happened: on the load a browser redirect comes back to, or in place when a
  // shell's deep link lands. An intent with no sign-in after it is left alone,
  // since that is someone who backed out of the provider's page.
  const lastSignInAt = oauth.lastSignInAt.value
  useEffect(() => {
    if (management !== true || lastSignInAt === null) {
      return
    }
    const intent = readIntent()
    if (intent === null || lastSignInAt < intent.createdAt) {
      return
    }
    clearIntent()
    start(intent, true)
    // `start` closes over state setters only; re-running on its identity would
    // repeat a destructive request.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [management, lastSignInAt])

  if (management === null) {
    return null
  }
  const canLink = oauth.canBindApple.value
  const linkButton = canLink && (
    <button
      type="button"
      disabled={busy !== null}
      onClick={() => start({ kind: 'link-apple' })}
    >
      {busy === 'link' ? 'Linking...' : 'Link an Apple ID'}
    </button>
  )
  const feedback = (
    <>
      {handedOff && error === null && (
        <p class="muted" role="status">
          Continue signing in in your browser, then come back. The change
          finishes once you have signed in.
        </p>
      )}
      {error !== null && (
        <p class="error" role="alert">
          {error}
        </p>
      )}
    </>
  )

  if (!management) {
    if (!canLink) {
      return null
    }
    if (oauth.sessionProvider.value === 'apple') {
      return (
        <section class="panel">
          <h2>Sign in with Apple</h2>
          <p class="muted" role="status">
            You are signed in with Apple, so your Apple ID is linked to this
            server. The link is to your Apple ID, not your email address.
          </p>
        </section>
      )
    }
    return (
      <section class="panel">
        <h2>Sign in with Apple</h2>
        <p class="muted">
          Link an Apple ID to this Axon server so you can sign in with Apple
          here and on your other devices. Linking also signs you in with that
          Apple ID. The link is to your Apple ID, not your email address.
        </p>
        {linkButton}
        {feedback}
      </section>
    )
  }

  const listed = identities ?? []
  if (listed.length === 0 && !canLink && loadError === null) {
    return null
  }

  return (
    <section class="panel">
      <h2>Linked sign-ins</h2>
      <p class="muted">
        The accounts that can sign in to this Axon server. A link is to the
        account itself, not to its email address.
      </p>
      {loadError !== null && (
        <p class="muted">Could not load linked sign-ins: {loadError}</p>
      )}
      {identities !== null && listed.length === 0 && (
        <p class="muted">No sign-ins are linked.</p>
      )}
      <ul class="linked-sign-ins">
        {listed.map((identity) => {
          const name = providerName(identity.provider)
          const account =
            identity.email != null
              ? `${name} (${identity.email})`
              : `this ${name} account`
          const asking =
            confirming?.identityId === identity.id ? confirming : null
          return (
            <li key={identity.id}>
              <div class="linked-sign-in-row">
                <div>
                  <strong>{name}</strong>
                  {identity.email != null && <> · {identity.email}</>}
                  <div class="muted">
                    Linked {new Date(identity.linked_at).toLocaleDateString()}
                    {identity.current && ' · This session'}
                    {!identity.sign_in_available &&
                      ' · Sign-in is switched off on this server'}
                  </div>
                </div>
                {asking === null && (
                  <button
                    type="button"
                    class="danger"
                    disabled={busy !== null}
                    aria-label={`Unlink ${name}${identity.email != null ? ` ${identity.email}` : ''}`}
                    onClick={() => {
                      setError(null)
                      setConfirming({
                        identityId: identity.id,
                        lastCredential: false,
                      })
                    }}
                  >
                    {busy === identity.id ? 'Unlinking...' : 'Unlink'}
                  </button>
                )}
              </div>
              {asking !== null && (
                <div class="linked-sign-in-confirm" role="group">
                  {asking.lastCredential ? (
                    <p role="alert">
                      <strong>This is the last way to sign in.</strong> Nothing
                      has been changed yet. If you unlink it, nobody can sign in
                      to this server from an app again until a new token is
                      issued on the server itself (
                      <code>axon-server token issue</code>).
                    </p>
                  ) : (
                    <p>
                      After this, {account} can no longer sign in to this Axon
                      server, and every device signed in with it is signed out
                      {identity.current && ', including this one'}. You can
                      still sign in any other way listed here.
                    </p>
                  )}
                  <p class="muted">
                    This only changes Axon. {name} may keep showing Axon in its
                    own list of apps you have signed in to, which gives Axon no
                    access once the sign-in is unlinked here.{' '}
                    {providerControls(identity.provider)}
                  </p>
                  <button
                    type="button"
                    class="danger"
                    disabled={busy !== null}
                    onClick={() =>
                      start({
                        kind: 'unlink',
                        identityId: identity.id,
                        allowLockout: asking.lastCredential,
                        current: identity.current,
                      })
                    }
                  >
                    {busy === identity.id
                      ? 'Unlinking...'
                      : asking.lastCredential
                        ? 'Unlink anyway'
                        : `Unlink ${name}`}
                  </button>{' '}
                  <button
                    type="button"
                    disabled={busy !== null}
                    onClick={() => {
                      // Also withdraws a confirmation that is still waiting
                      // on a sign-in the user backed out of.
                      clearIntent()
                      setConfirming(null)
                    }}
                  >
                    Cancel
                  </button>
                </div>
              )}
            </li>
          )
        })}
      </ul>
      {linkButton}
      {feedback}
    </section>
  )
}
