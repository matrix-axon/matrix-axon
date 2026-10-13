import { useEffect, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import { CopyableText, useCopyFeedback } from '../../components/CopyableText'
import { ErrorBanner } from '../../components/ErrorBanner'
import { useServices } from '../../services'
import { hasActiveAccount } from '../../stores/accounts'
import {
  isTerminalMatrixOAuthBrowserFlow,
  type MatrixOAuthBrowserFlow,
} from '../../stores/matrix-oauth-browser'
import { safeVerificationUri } from './MatrixOAuthQrAcquisition'

const STAGE_SUMMARIES: Record<MatrixOAuthBrowserFlow['stage'], string> = {
  starting: 'Asking your homeserver for a sign-in link…',
  waiting_for_authorization:
    'Open your homeserver’s sign-in page, sign in, and approve this device.',
  finalizing: 'Approved. Finishing sign-in…',
  done: 'Signed in.',
  failed: 'Browser sign-in failed.',
  cancelled: 'Browser sign-in cancelled.',
}

function CopyLinkButton({ link }: { link: string }) {
  const { status, copy } = useCopyFeedback()
  return (
    <button type="button" onClick={() => void copy(link)}>
      {status === 'copied'
        ? 'Copied'
        : status === 'failed'
          ? 'Could not copy'
          : 'Copy link'}
    </button>
  )
}

function AuthorizationStep({ flow }: { flow: MatrixOAuthBrowserFlow }) {
  const link = safeVerificationUri(flow.verification_uri)
  const code = flow.user_code ?? null
  return (
    <div class="browser-sign-in-step">
      {link !== null ? (
        <>
          <p>
            {/* A link the user clicks, never one opened for them: it arrives
                after a request, where a popup blocker would swallow it. In
                the shells the external-link handler passes it to the system
                browser. */}
            <a
              class="button-like"
              href={link}
              target="_blank"
              rel="noopener noreferrer"
            >
              Open sign-in page
            </a>{' '}
            <CopyLinkButton link={link} />
          </p>
          <p class="field-hint">
            Opens <strong>{new URL(link).host}</strong> in your browser. To
            approve on another device, copy the link and open it there. Only
            open it yourself — anyone who can sign in as you there could approve
            this device.
          </p>
        </>
      ) : flow.verification_uri !== null &&
        flow.verification_uri !== undefined ? (
        <p class="field-hint error">
          Axon returned an unsafe sign-in link. Cancel and contact your
          homeserver’s operator.
        </p>
      ) : null}
      {code !== null && (
        <div class="browser-sign-in-code">
          {/* Always shown: some servers put the code in the link and only ask
              the user to confirm it, others return a bare link and ask for it
              to be typed. Nothing tells the client which. */}
          <p>Confirm or enter this code when the page asks for it:</p>
          <CopyableText text={code} label="sign-in code">
            <code class="authorization-code">{code}</code>
          </CopyableText>
        </div>
      )}
      <p class="field-hint" role="status">
        Waiting for approval. Keep this page open; it continues by itself.
      </p>
    </div>
  )
}

function SignedIn({
  flow,
  firstAccount,
}: {
  flow: MatrixOAuthBrowserFlow
  firstAccount: boolean
}) {
  const { accounts, verification, matrixOAuthBrowser } = useServices()
  const location = useLocation()
  const account = accounts.accounts.value.find(
    (candidate) => candidate.account_id === flow.account_id,
  )
  const verified = account?.verified === true
  return (
    <div class="browser-sign-in-step">
      {verified ? (
        <p>This Axon device is verified.</p>
      ) : (
        <>
          <p>
            {flow.expected_user_id} is signed in, but this Axon device is{' '}
            <strong>not verified yet</strong>. Until it is, encrypted messages
            will not decrypt.
          </p>
          <p>
            Verify it with another signed-in Matrix device, or choose{' '}
            <strong>Recover keys</strong> on the account above and enter your
            recovery key.
          </p>
        </>
      )}
      <div class="card-actions">
        {!verified && account !== undefined && (
          <button
            type="button"
            onClick={() =>
              verification.openPicker({
                accountId: account.account_id,
                ownDeviceId:
                  account.device_id === undefined ||
                  account.device_id === null ||
                  account.device_id === ''
                    ? null
                    : account.device_id,
              })
            }
          >
            Verify with another device
          </button>
        )}
        {firstAccount && (
          <button type="button" onClick={() => location.route('/')}>
            Go to rooms
          </button>
        )}
        <button type="button" onClick={() => matrixOAuthBrowser.reset()}>
          Add another account
        </button>
      </div>
    </div>
  )
}

function MismatchedBrowserFlow({
  flow,
  expectedUserId,
}: {
  flow: MatrixOAuthBrowserFlow
  expectedUserId: string
}) {
  const { matrixOAuthBrowser } = useServices()
  const cancelling = matrixOAuthBrowser.operation.value === 'cancelling'
  return (
    <div class="qr-flow">
      <p class="qr-stage-summary" role="status">
        Browser sign-in for {flow.expected_user_id} is still open. Cancel it
        before signing in again as {expectedUserId}.
      </p>
      <ErrorBanner error={matrixOAuthBrowser.error} />
      <button
        type="button"
        disabled={cancelling}
        onClick={() =>
          void matrixOAuthBrowser.cancel().then((cancelled) => {
            if (cancelled) {
              matrixOAuthBrowser.reset()
            }
          })
        }
      >
        {cancelling ? 'Cancelling…' : 'Cancel previous browser sign-in'}
      </button>
    </div>
  )
}

/**
 * Sign in through the homeserver's own browser page (ADR 0117).
 *
 * Unlike QR sign-in this leaves the account unverified, so a finished flow
 * stays on screen and points at verification instead of navigating away.
 */
export function MatrixOAuthBrowserAcquisition({
  expectedUserId,
  onSuccess,
}: {
  expectedUserId?: string
  onSuccess?: () => void
}) {
  const { accounts, matrixOAuthBrowser } = useServices()
  const [userId, setUserId] = useState('')
  const completionHandled = useRef<string | null>(null)
  // Whether the sign-in began with no active account, decided before the new
  // account can appear in the list. Null until the list has loaded.
  const [firstAccount, setFirstAccount] = useState<boolean | null>(null)
  const flow = matrixOAuthBrowser.flow.value
  const operation = matrixOAuthBrowser.operation.value
  const effectiveUserId = expectedUserId ?? userId
  const flowMatchesExpectedUser =
    flow === null ||
    expectedUserId === undefined ||
    flow.expected_user_id === expectedUserId

  useEffect(() => {
    void matrixOAuthBrowser.resume()
  }, [matrixOAuthBrowser])

  // The user approves in another tab or app. Timers in a hidden page are
  // throttled or frozen, so ask as soon as they come back.
  useEffect(() => {
    const onVisible = () => {
      if (document.visibilityState === 'visible') {
        matrixOAuthBrowser.checkNow()
      }
    }
    document.addEventListener('visibilitychange', onVisible)
    return () => document.removeEventListener('visibilitychange', onVisible)
  }, [matrixOAuthBrowser])

  useEffect(() => {
    if (flow === null) {
      if (operation !== 'starting') {
        setFirstAccount(null)
      }
      return
    }
    if (
      firstAccount === null &&
      !accounts.loading.value &&
      flow.stage !== 'done'
    ) {
      setFirstAccount(!hasActiveAccount(accounts.accounts.value))
    }
    if (
      flow.stage === 'done' &&
      flowMatchesExpectedUser &&
      completionHandled.current !== flow.flow_id
    ) {
      completionHandled.current = flow.flow_id
      onSuccess?.()
    }
  }, [
    accounts.accounts.value,
    accounts.loading.value,
    firstAccount,
    flow,
    flowMatchesExpectedUser,
    onSuccess,
    operation,
  ])

  if (flow !== null) {
    if (!flowMatchesExpectedUser && expectedUserId !== undefined) {
      return (
        <MismatchedBrowserFlow flow={flow} expectedUserId={expectedUserId} />
      )
    }
    return (
      <div class="qr-flow browser-sign-in">
        <p class="qr-stage-summary" aria-live="polite" aria-atomic="true">
          {STAGE_SUMMARIES[flow.stage]}
        </p>
        {flow.stage === 'waiting_for_authorization' && (
          <AuthorizationStep flow={flow} />
        )}
        <ErrorBanner error={matrixOAuthBrowser.error} />
        {flow.stage === 'done' && operation === 'idle' && (
          <SignedIn flow={flow} firstAccount={firstAccount === true} />
        )}
        {!isTerminalMatrixOAuthBrowserFlow(flow) && (
          <button
            type="button"
            disabled={operation === 'cancelling'}
            onClick={() => void matrixOAuthBrowser.cancel()}
          >
            {operation === 'cancelling' ? 'Cancelling…' : 'Cancel sign-in'}
          </button>
        )}
        {(flow.stage === 'failed' || flow.stage === 'cancelled') && (
          <button type="button" onClick={() => matrixOAuthBrowser.reset()}>
            Start again
          </button>
        )}
      </div>
    )
  }

  const starting = operation === 'starting'
  const resuming = operation === 'resuming'
  return (
    <form
      class="stack-form"
      onSubmit={(event) => {
        event.preventDefault()
        setFirstAccount(
          accounts.loading.value
            ? null
            : !hasActiveAccount(accounts.accounts.value),
        )
        void matrixOAuthBrowser.start(effectiveUserId.trim())
      }}
    >
      <p class="muted">
        For homeservers where you sign in on a web page — single sign-on, a
        passkey, or a second factor. Axon gives you a link to that page and
        waits while you approve.
      </p>
      <label>
        Matrix user ID
        <input
          value={effectiveUserId}
          placeholder="@alice:example.org"
          readOnly={expectedUserId !== undefined}
          disabled={starting || resuming}
          onInput={(event) => setUserId(event.currentTarget.value)}
        />
      </label>
      <button
        type="submit"
        disabled={effectiveUserId.trim() === '' || starting || resuming}
      >
        {starting
          ? 'Preparing…'
          : resuming
            ? 'Checking…'
            : 'Start browser sign-in'}
      </button>
      <ErrorBanner error={matrixOAuthBrowser.error} />
    </form>
  )
}
