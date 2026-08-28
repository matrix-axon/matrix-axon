import { signal } from '@preact/signals'
import { useCallback, useEffect, useMemo, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import {
  BACKUP_ALREADY_UPLOADING_HINT,
  backupBadge,
  backupSnapshotLines,
  enableBackupNotice,
  recoverHonestyNotice,
} from '../backup'
import { CopyableText } from '../components/CopyableText'
import { ErrorBanner } from '../components/ErrorBanner'
import { NoticeBanner, type Notice } from '../components/NoticeBanner'
import { isValidRecoveryKey } from '../recovery-key'
import { useServices } from '../services'
import {
  hasActiveAccount,
  type Account,
  type SyncState,
} from '../stores/accounts'
import { isTerminalMatrixOAuthQrFlow } from '../stores/matrix-oauth-qr'
import { ServerStatus } from './ServerStatus'
import { MatrixOAuthQrAcquisition } from './accounts/MatrixOAuthQrAcquisition'

/**
 * The account lifecycle page (ADR 0046, M-W3): list with state and
 * sync-readiness, login (add or reactivate), logout, recover, megolm backup
 * snapshot and enable (ADR 0098), delete-with-confirm, and the active-account
 * switch persisted in settings.
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

function Badge({
  className,
  title,
  children,
}: {
  className: string
  title: string
  children: string
}) {
  return (
    <span class={`badge ${className}`} title={title} aria-label={title}>
      {children}
    </span>
  )
}

const RECOVER_KEYS_HINT =
  'Import Matrix Server-Side Secret Storage (4S) with a recovery key. That unlocks megolm backup so stored encrypted messages can decrypt, and verifies this Axon device when cross-signing keys are present. Messages stay undecryptable if those session keys were never uploaded to backup.'

const LOG_OUT_HINT =
  'Log out of Axon while keeping the local archive. Sign in again later to resume. A new login creates a fresh Matrix device.'

const DELETE_HINT =
  'Permanently remove this account and its Axon data. The Matrix account on the homeserver is not affected.'

function accountStateTitle(state: Account['state']): string {
  switch (state) {
    case 'active':
      return 'Logged in to Axon. Messages sync while this account stays active.'
    case 'deactivated':
      return 'Logged out of Axon. The local archive is kept until you sign in again or delete the account.'
    case 'deleting':
      return 'Axon is removing this account and its local data.'
  }
}

function syncStateTitle(state: Exclude<SyncState, 'ready'>): string {
  switch (state) {
    case 'offline':
      return 'Axon lost the homeserver connection and is retrying. Sending may fail until it reconnects.'
    case 'connecting':
      return 'Sync is starting. Sending may be unreliable until the first cycle finishes.'
    case 'syncing':
      return 'First sync has not finished. Sending may be unreliable until it completes.'
  }
}

/** Sync-readiness per ADR 0030 — hidden once the account is `ready`. */
function SyncBadge({ account }: { account: Account }) {
  const state = account.sync_state
  if (state === 'ready' || state === undefined) {
    return null
  }
  return (
    <Badge className={`sync-${state}`} title={syncStateTitle(state)}>
      {state}
    </Badge>
  )
}

function AccountCard({
  account,
  onReactivate,
}: {
  account: Account
  onReactivate: (account: Account) => void
}) {
  const { accounts, settings, api } = useServices()
  const [confirmingDelete, setConfirmingDelete] = useState(false)
  const [secretForm, setSecretForm] = useState<'recover' | 'enable' | null>(
    null,
  )
  const [secretKey, setSecretKey] = useState('')
  const [displayName, setDisplayName] = useState<string | null>(null)
  // Per-card recover / enable outcome, held in a signal so it survives the
  // form closing on success; `NoticeBanner` reads and dismisses it (same idiom
  // as the store error signals feeding `ErrorBanner`).
  const notice = useMemo(() => signal<Notice | null>(null), [])

  const id = account.account_id
  const pending = accounts.pending.value
  const busy = pending !== null
  const recovering = pending?.kind === 'recover' && pending.accountId === id
  const enabling = pending?.kind === 'enable-backup' && pending.accountId === id
  const isActiveChoice = settings.activeAccountId.value === id
  const keyValid = isValidRecoveryKey(secretKey)
  const enableKeyOk = secretKey.trim() === '' || keyValid
  const snapshot = account.backup
  const badge = snapshot !== undefined ? backupBadge(snapshot) : null
  const snapshotLines =
    snapshot !== undefined ? backupSnapshotLines(snapshot) : null
  const alreadyUploading = snapshot?.this_device_uploading === true

  const openSecretForm = (kind: 'recover' | 'enable') => {
    notice.value = null
    if (secretForm === kind) {
      setSecretForm(null)
      setSecretKey('')
      return
    }
    setSecretForm(kind)
    setSecretKey('')
  }

  const closeSecretForm = () => {
    setSecretForm(null)
    setSecretKey('')
  }

  useEffect(() => {
    // Deactivated/deleting accounts have no live homeserver session;
    // GET profile always 503s (`Account not reachable`).
    if (account.state !== 'active') {
      setDisplayName(null)
      return
    }
    let cancelled = false
    void (async () => {
      try {
        const { data, error } = await api.GET(
          '/v1/accounts/{account_id}/users/{user_id}/profile',
          {
            params: {
              path: {
                account_id: account.account_id,
                user_id: account.user_id,
              },
            },
          },
        )
        if (cancelled || error !== undefined) {
          return
        }
        const name = data.data.display_name?.trim() ?? ''
        if (name !== '' && name !== account.user_id) {
          setDisplayName(name)
        }
      } catch {
        // Best-effort: the MXID is already on the card.
      }
    })()
    return () => {
      cancelled = true
    }
  }, [api, account.account_id, account.user_id, account.state])

  return (
    <li
      class={`card account-${account.state}`}
      aria-busy={recovering || enabling}
    >
      <div class="card-head">
        <CopyableText text={account.user_id} label="user ID">
          <strong>{account.user_id}</strong>
        </CopyableText>
        <Badge
          className={`state-${account.state}`}
          title={accountStateTitle(account.state)}
        >
          {account.state}
        </Badge>
        {account.verified === true && (
          <Badge
            className="verified"
            title="Axon's Matrix device is cross-signed. This is independent of megolm key backup."
          >
            verified
          </Badge>
        )}
        {badge !== null && (
          <Badge className={badge.className} title={badge.title}>
            {badge.label}
          </Badge>
        )}
        {account.state === 'active' && <SyncBadge account={account} />}
      </div>
      {displayName !== null && (
        <div class="card-meta account-display-name">{displayName}</div>
      )}
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
      {snapshotLines !== null && (
        <div class="card-meta backup-snapshot">
          <div>{snapshotLines.megolm}</div>
          <div>{snapshotLines.secretStorage}</div>
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
              title={RECOVER_KEYS_HINT}
              onClick={() => openSecretForm('recover')}
            >
              Recover keys
            </button>
            {account.verified === true && (
              <button
                type="button"
                class={alreadyUploading ? 'quiet' : undefined}
                disabled={busy}
                title={
                  alreadyUploading ? BACKUP_ALREADY_UPLOADING_HINT : undefined
                }
                onClick={() => openSecretForm('enable')}
              >
                Enable backup
              </button>
            )}
            <button
              type="button"
              disabled={busy}
              title={LOG_OUT_HINT}
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
              title={DELETE_HINT}
              onClick={() => setConfirmingDelete(true)}
            >
              Delete
            </button>
          ))}
      </div>

      {secretForm === 'recover' && account.state === 'active' && (
        <>
          <form
            class="inline-form"
            onSubmit={(event) => {
              event.preventDefault()
              void accounts.recover(id, secretKey).then((result) => {
                if (!result.ok) {
                  return
                }
                closeSecretForm()
                notice.value = recoverHonestyNotice(
                  result.verified,
                  result.backupAction,
                )
              })
            }}
          >
            <label>
              Recovery key
              <input
                type="password"
                value={secretKey}
                placeholder="EsTc …"
                disabled={recovering}
                onInput={(event) => setSecretKey(event.currentTarget.value)}
              />
            </label>
            <button type="submit" disabled={busy || !keyValid}>
              {recovering ? 'Recovering…' : 'Recover'}
            </button>
          </form>
          {recovering && (
            <p class="field-hint" role="status" aria-live="polite">
              Importing Matrix Server-Side Secret Storage (4S) — this can take a
              while.
            </p>
          )}
          {secretKey.trim() !== '' && !keyValid && (
            <p class="field-hint error">
              That doesn&rsquo;t look like a valid recovery key — check for a
              missing or extra character.
            </p>
          )}
        </>
      )}
      {secretForm === 'enable' && account.state === 'active' && (
        <>
          <form
            class="inline-form"
            onSubmit={(event) => {
              event.preventDefault()
              void accounts.enableBackup(id, secretKey).then((result) => {
                if (!result.ok || result.backupAction === undefined) {
                  return
                }
                closeSecretForm()
                notice.value = enableBackupNotice(result.backupAction)
              })
            }}
          >
            <label>
              Recovery key
              <input
                type="password"
                value={secretKey}
                placeholder="EsTc …"
                disabled={enabling}
                onInput={(event) => setSecretKey(event.currentTarget.value)}
              />
            </label>
            <button type="submit" disabled={busy || !enableKeyOk}>
              {enabling ? 'Enabling…' : 'Enable'}
            </button>
            <button type="button" disabled={enabling} onClick={closeSecretForm}>
              Cancel
            </button>
          </form>
          {enabling ? (
            <p class="field-hint" role="status" aria-live="polite">
              Enabling megolm backup — this can take a while.
            </p>
          ) : (
            <p class="field-hint">
              Leave blank to retry an already-enabled upload.
            </p>
          )}
          {secretKey.trim() !== '' && !keyValid && (
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
  const notice = useMemo(() => signal<Notice | null>(null), [])
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
            .then((result) => {
              setPassword('')
              setRecoveryKey('')
              if (result.ok) {
                setUsername('')
                setHomeserver('')
                onSuccess()
                if (firstActiveLogin) {
                  location.route('/')
                  return
                }
                if (result.recover?.ok === true) {
                  notice.value = recoverHonestyNotice(
                    result.recover.verified,
                    result.recover.backupAction,
                  )
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
            disabled={busy}
            onInput={(event) => setUsername(event.currentTarget.value)}
          />
        </label>
        <label>
          Password
          <input
            ref={passwordInput}
            type="password"
            value={password}
            disabled={busy}
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
            disabled={busy}
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
              disabled={busy}
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
          {accounts.pending.value?.kind === 'login'
            ? reactivation === null
              ? 'Logging in…'
              : 'Reactivating…'
            : reactivation === null
              ? 'Log in'
              : 'Reactivate account'}
        </button>
      </form>
      <p class="muted">
        The password authenticates once with the homeserver and is never stored.
        Logging in with a logged-out account's user ID reactivates it.
      </p>
      <NoticeBanner notice={notice} />
    </div>
  )
}
