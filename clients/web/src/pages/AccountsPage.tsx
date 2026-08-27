import { signal } from '@preact/signals'
import { useCallback, useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import { CopyableText } from '../components/CopyableText'
import { ErrorBanner } from '../components/ErrorBanner'
import { NoticeBanner, type Notice } from '../components/NoticeBanner'
import { isValidRecoveryKey } from '../recovery-key'
import { useServices } from '../services'
import { hasActiveAccount, type Account } from '../stores/accounts'
import { isTerminalMatrixOAuthQrFlow } from '../stores/matrix-oauth-qr'
import { ServerStatus } from './ServerStatus'
import { MatrixOAuthQrAcquisition } from './accounts/MatrixOAuthQrAcquisition'

/**
 * The account lifecycle page (ADR 0046, M-W3): list with state and
 * sync-readiness, login (add or reactivate), logout, recover,
 * delete-with-confirm, and the active-account switch persisted in settings.
 */
export function AccountsPage() {
  const { accounts, matrixOAuthQr } = useServices()
  const [acquisition, setAcquisition] = useState<{
    reactivation: Account | null
    method: 'password' | 'qr'
  }>({ reactivation: null, method: 'password' })
  const finishReactivation = useCallback(
    () =>
      setAcquisition((current) => ({
        ...current,
        reactivation: null,
      })),
    [],
  )
  const beginReactivation = useCallback(
    (account: Account) =>
      setAcquisition({ reactivation: account, method: 'password' }),
    [],
  )
  const cancelReactivation = useCallback(async () => {
    const current = matrixOAuthQr.flow.value
    if (
      current !== null &&
      !isTerminalMatrixOAuthQrFlow(current) &&
      !(await matrixOAuthQr.cancel())
    ) {
      return
    }
    matrixOAuthQr.reset()
    finishReactivation()
  }, [finishReactivation, matrixOAuthQr])

  useEffect(() => {
    void accounts.refresh()
  }, [accounts])

  return (
    <div class="page accounts-page">
      <h1>Accounts</h1>
      <p class="muted">
        Add Matrix accounts, choose the active account, and manage verification
        and recovery from one place.
      </p>
      <ErrorBanner error={accounts.error} />
      {accounts.loading.value ? (
        <p>Loading accounts…</p>
      ) : accounts.accounts.value.length === 0 ? (
        <p>No accounts yet — add one below.</p>
      ) : (
        <ul class="cards">
          {accounts.accounts.value.map((account) => (
            <AccountCard
              key={account.account_id}
              account={account}
              onReactivate={beginReactivation}
            />
          ))}
        </ul>
      )}
      <AccountAcquisition
        reactivation={acquisition.reactivation}
        method={acquisition.method}
        onMethodChange={(method) =>
          setAcquisition((current) => ({ ...current, method }))
        }
        onCancelReactivation={cancelReactivation}
        onSuccess={finishReactivation}
      />
      <ServerStatus />
    </div>
  )
}

/** Sync-readiness per ADR 0030 — hidden once the account is `ready`. */
function SyncBadge({ account }: { account: Account }) {
  const state = account.sync_state
  if (state === 'ready') {
    return null
  }
  const hint =
    state === 'offline'
      ? 'server lost the homeserver connection and is retrying'
      : 'first sync not finished — sending may be unreliable'
  return (
    <span class={`badge sync-${state}`} title={hint}>
      {state}
    </span>
  )
}

function AccountCard({
  account,
  onReactivate,
}: {
  account: Account
  onReactivate: (account: Account) => void
}) {
  const { accounts, settings } = useServices()
  const [confirmingDelete, setConfirmingDelete] = useState(false)
  const [recoverKey, setRecoverKey] = useState<string | null>(null)
  // Per-card recovery outcome, held in a signal so it survives the form closing
  // on success; `NoticeBanner` reads and dismisses it (same idiom as the store
  // error signals feeding `ErrorBanner`).
  const notice = useMemo(() => signal<Notice | null>(null), [])

  const id = account.account_id
  const pending = accounts.pending.value
  const busy = pending !== null
  const isActiveChoice = settings.activeAccountId.value === id
  const keyValid = recoverKey !== null && isValidRecoveryKey(recoverKey)

  return (
    <li class={`card account-${account.state}`}>
      <div class="card-head">
        <CopyableText text={account.user_id} label="user ID">
          <strong>{account.user_id}</strong>
        </CopyableText>
        <span class={`badge state-${account.state}`}>{account.state}</span>
        {account.verified === true && (
          <span class="badge verified" title="axon's device is cross-signed">
            verified
          </span>
        )}
        {account.state === 'active' && <SyncBadge account={account} />}
      </div>
      <div class="card-meta">
        <CopyableText text={id} label="account ID">
          <code>{id}</code>
        </CopyableText>
      </div>
      {account.homeserver_url !== '' && (
        <div class="card-meta">
          <CopyableText text={account.homeserver_url} label="homeserver URL">
            {account.homeserver_url}
          </CopyableText>
        </div>
      )}

      <div class="card-actions">
        {account.state === 'deactivated' && (
          <button
            type="button"
            disabled={busy}
            onClick={() => onReactivate(account)}
          >
            Sign in again
          </button>
        )}
        {account.state === 'active' && (
          <>
            <label class="switch">
              <input
                type="radio"
                name="active-account"
                checked={isActiveChoice}
                onChange={() => (settings.activeAccountId.value = id)}
              />
              use this account
            </label>
            <button
              type="button"
              disabled={busy}
              onClick={() => {
                notice.value = null
                setRecoverKey(recoverKey === null ? '' : null)
              }}
            >
              Recover keys
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={() => void accounts.logout(id)}
            >
              Log out
            </button>
          </>
        )}
        {account.state !== 'deleting' &&
          (confirmingDelete ? (
            <span class="confirm">
              Delete {account.user_id} from axon permanently?
              <button
                type="button"
                class="danger"
                disabled={busy}
                onClick={() => {
                  void accounts
                    .remove(id)
                    .then(() => setConfirmingDelete(false))
                }}
              >
                Confirm delete
              </button>
              <button type="button" onClick={() => setConfirmingDelete(false)}>
                Cancel
              </button>
            </span>
          ) : (
            <button
              type="button"
              class="danger"
              disabled={busy}
              onClick={() => setConfirmingDelete(true)}
            >
              Delete
            </button>
          ))}
      </div>

      {recoverKey !== null && account.state === 'active' && (
        <>
          <form
            class="inline-form"
            onSubmit={(event) => {
              event.preventDefault()
              void accounts.recover(id, recoverKey).then((result) => {
                if (!result.ok) {
                  return
                }
                setRecoverKey(null)
                notice.value = result.verified
                  ? {
                      tone: 'success',
                      message: 'Keys recovered — this device is now verified.',
                    }
                  : {
                      tone: 'info',
                      message:
                        'Keys recovered. This device is still unverified — the ' +
                        'recovery data may not include cross-signing keys.',
                    }
              })
            }}
          >
            <label>
              Recovery key
              <input
                type="password"
                value={recoverKey}
                placeholder="EsTc …"
                onInput={(event) => setRecoverKey(event.currentTarget.value)}
              />
            </label>
            <button type="submit" disabled={busy || !keyValid}>
              Recover
            </button>
          </form>
          {recoverKey.trim() !== '' && !keyValid && (
            <p class="field-hint error">
              That doesn&rsquo;t look like a valid recovery key — check for a
              missing or extra character.
            </p>
          )}
        </>
      )}
      <NoticeBanner notice={notice} />
    </li>
  )
}

function AccountAcquisition({
  reactivation,
  method,
  onMethodChange,
  onCancelReactivation,
  onSuccess,
}: {
  reactivation: Account | null
  method: 'password' | 'qr'
  onMethodChange: (method: 'password' | 'qr') => void
  onCancelReactivation: () => Promise<void>
  onSuccess: () => void
}) {
  const { matrixOAuthQr } = useServices()
  const section = useRef<HTMLElement>(null)

  useEffect(() => {
    if (reactivation === null) {
      return
    }
    section.current?.scrollIntoView?.({ block: 'start' })
  }, [reactivation])

  return (
    <section
      ref={section}
      class="account-add panel"
      aria-labelledby="add-account-heading"
    >
      <h2 id="add-account-heading">
        {reactivation === null
          ? 'Add account'
          : `Reactivate ${reactivation.user_id}`}
      </h2>
      {reactivation !== null && (
        <>
          <p class="muted">
            Sign in again with this account&rsquo;s password or a trusted Matrix
            device. Axon will reuse its stored homeserver.
          </p>
          <div class="card-actions">
            <button
              type="button"
              disabled={matrixOAuthQr.operation.value === 'cancelling'}
              onClick={() => void onCancelReactivation()}
            >
              {matrixOAuthQr.operation.value === 'cancelling'
                ? 'Cancelling reactivation…'
                : 'Cancel reactivation'}
            </button>
          </div>
        </>
      )}
      <div
        class="acquisition-methods"
        role="tablist"
        aria-label="Sign-in method"
      >
        <button
          type="button"
          role="tab"
          aria-selected={method === 'password'}
          onClick={() => onMethodChange('password')}
        >
          Sign in with password
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={method === 'qr'}
          onClick={() => onMethodChange('qr')}
        >
          Sign in with QR code
        </button>
      </div>
      <div role="tabpanel">
        {method === 'password' ? (
          <>
            <ErrorBanner error={matrixOAuthQr.error} />
            <PasswordAccountAcquisition
              reactivation={reactivation}
              onSuccess={onSuccess}
            />
          </>
        ) : (
          <MatrixOAuthQrAcquisition
            expectedUserId={reactivation?.user_id}
            onSuccess={onSuccess}
          />
        )}
      </div>
    </section>
  )
}

function PasswordAccountAcquisition({
  reactivation,
  onSuccess,
}: {
  reactivation: Account | null
  onSuccess: () => void
}) {
  const { accounts } = useServices()
  const location = useLocation()
  const passwordInput = useRef<HTMLInputElement>(null)
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [recoveryKey, setRecoveryKey] = useState('')
  const [homeserver, setHomeserver] = useState('')
  const busy = accounts.pending.value !== null
  const effectiveUsername = reactivation?.user_id ?? username
  // The key is optional here (SAS or a later recover can supply it), so an
  // empty field is fine; a non-empty one must be well-formed before submit.
  const keyOk = recoveryKey.trim() === '' || isValidRecoveryKey(recoveryKey)

  useEffect(() => {
    setUsername('')
    setPassword('')
    setRecoveryKey('')
    setHomeserver('')
    if (reactivation !== null) {
      passwordInput.current?.focus()
    }
  }, [reactivation])

  return (
    <div class="password-acquisition">
      <form
        class="stack-form"
        onSubmit={(event) => {
          event.preventDefault()
          const firstActiveLogin = !hasActiveAccount(accounts.accounts.value)
          void accounts
            .login({
              username: effectiveUsername.trim(),
              password,
              recovery_key:
                recoveryKey.trim() === '' ? undefined : recoveryKey.trim(),
              homeserver_url:
                reactivation !== null || homeserver.trim() === ''
                  ? undefined
                  : homeserver.trim(),
            })
            .then((ok) => {
              setPassword('')
              setRecoveryKey('')
              if (ok) {
                setUsername('')
                setHomeserver('')
                onSuccess()
                if (firstActiveLogin) {
                  location.route('/')
                }
              }
            })
        }}
      >
        <label>
          Matrix user ID
          <input
            value={effectiveUsername}
            placeholder="@alice:example.org"
            readOnly={reactivation !== null}
            onInput={(event) => setUsername(event.currentTarget.value)}
          />
        </label>
        <label>
          Password
          <input
            ref={passwordInput}
            type="password"
            value={password}
            onInput={(event) => setPassword(event.currentTarget.value)}
          />
        </label>
        <label>
          Matrix Recovery Key (optional; add later, or skip with SAS or QR code
          verification)
          <input
            type="password"
            value={recoveryKey}
            placeholder="EsTc …"
            onInput={(event) => setRecoveryKey(event.currentTarget.value)}
          />
        </label>
        {recoveryKey.trim() !== '' && !keyOk && (
          <p class="field-hint error">
            That doesn&rsquo;t look like a valid recovery key — check for a
            missing or extra character, or leave it blank to skip.
          </p>
        )}
        {reactivation === null && (
          <label>
            Homeserver URL{' '}
            <span class="muted">(optional — autodiscovered when omitted)</span>
            <input
              value={homeserver}
              placeholder="https://matrix.example.org"
              onInput={(event) => setHomeserver(event.currentTarget.value)}
            />
          </label>
        )}
        <button
          type="submit"
          disabled={
            busy || effectiveUsername.trim() === '' || password === '' || !keyOk
          }
        >
          {reactivation === null ? 'Log in' : 'Reactivate account'}
        </button>
      </form>
      <p class="muted">
        The password authenticates once with the homeserver and is never stored.
        Logging in with a logged-out account's user ID reactivates it.
      </p>
    </div>
  )
}
