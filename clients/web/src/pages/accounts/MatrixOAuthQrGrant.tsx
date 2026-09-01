import { useEffect, useMemo, useState } from 'preact/hooks'
import { ErrorBanner } from '../../components/ErrorBanner'
import { useServices } from '../../services'
import type { Account } from '../../stores/accounts'
import {
  isTerminalMatrixOAuthQrFlow,
  type MatrixOAuthQrGrantFlow,
} from '../../stores/matrix-oauth-qr'
import {
  MatrixOAuthQrFlowPanel,
  type MatrixOAuthQrFlowCopy,
} from './MatrixOAuthQrAcquisition'

const STAGE_SUMMARIES: Record<MatrixOAuthQrGrantFlow['stage'], string> = {
  starting: 'Preparing device authorization…',
  qr_ready: 'QR code ready. Scan it with the new Matrix client.',
  check_code_to_display:
    'Compare this check code with the code on the new Matrix client.',
  check_code_required:
    'Enter the two-digit check code shown by the new Matrix client.',
  waiting_for_authorization:
    'Approve the new device with your Matrix authorization service.',
  syncing_secrets:
    'Authorization approved. Axon is securely provisioning the new device…',
  done: 'The new Matrix device is authorized, verified, and provisioned.',
  failed: 'Device authorization failed.',
  cancelled: 'Device authorization cancelled.',
}

const FLOW_COPY: MatrixOAuthQrFlowCopy = {
  stageSummaries: STAGE_SUMMARIES,
  qrAriaLabel: 'Matrix device authorization QR code',
  scannerSource: 'the new Matrix client',
  cancel: 'Cancel device authorization',
  cancelling: 'Cancelling…',
  startAgain: 'Start again',
  doneAction: 'Authorize another device',
  unsafeVerificationLink:
    'Axon returned an unsafe authorization link. Open your Matrix authorization service directly instead.',
  authorizationGuidance:
    'Review the account and new device there, then explicitly approve it. This flow cannot finish without that approval.',
}

function grantEligible(account: Account): boolean {
  return account.state === 'active' && account.verified === true
}

export function MatrixOAuthQrGrant() {
  const { accounts, matrixOAuthQrGrant, settings } = useServices()
  const eligible = useMemo(
    () => accounts.accounts.value.filter(grantEligible),
    [accounts.accounts.value],
  )
  const defaultAccountId =
    eligible.find(
      (account) => account.account_id === settings.activeAccountId.value,
    )?.account_id ??
    eligible[0]?.account_id ??
    ''
  const [accountId, setAccountId] = useState(defaultAccountId)
  const [presentation, setPresentation] = useState<'display' | 'scan'>('scan')
  const [open, setOpen] = useState(false)
  const flow = matrixOAuthQrGrant.flow.value

  useEffect(() => {
    if (
      accountId === '' ||
      !eligible.some((account) => account.account_id === accountId)
    ) {
      setAccountId(defaultAccountId)
    }
  }, [accountId, defaultAccountId, eligible])

  useEffect(() => {
    if (!accounts.loading.value) {
      const accountIds = accounts.accounts.value.map(
        (account) => account.account_id,
      )
      if (accountIds.length === 0) {
        const current = matrixOAuthQrGrant.flow.value
        if (current !== null && !isTerminalMatrixOAuthQrFlow(current)) {
          void matrixOAuthQrGrant.cancel()
        } else {
          matrixOAuthQrGrant.reset()
        }
      } else {
        void matrixOAuthQrGrant.resume(accountIds)
      }
    }
  }, [accounts.accounts.value, accounts.loading.value, matrixOAuthQrGrant])

  const expectedAccount =
    flow === null
      ? eligible.find((account) => account.account_id === accountId)
      : accounts.accounts.value.find(
          (account) => account.account_id === flow.account_id,
        )

  if (!accounts.loading.value && accounts.accounts.value.length === 0) {
    return null
  }

  return (
    <section
      class="account-grant panel"
      aria-labelledby="authorize-device-heading"
    >
      <h2 id="authorize-device-heading">Authorize another Matrix device</h2>
      <p class="muted">
        Use this trusted Axon account to sign in and verify a new client such as
        Element X. The Matrix access tokens and encryption secrets stay inside
        Axon and never reach this browser.
      </p>

      {flow !== null ? (
        <>
          <ExpectedAccountGuidance account={expectedAccount} />
          <MatrixOAuthQrFlowPanel
            flow={flow}
            store={matrixOAuthQrGrant}
            copy={FLOW_COPY}
          />
        </>
      ) : eligible.length === 0 ? (
        <>
          <p>
            No account is ready to authorize another device. The Axon account
            must be active and its current Matrix device must be verified.
          </p>
          <ErrorBanner error={matrixOAuthQrGrant.error} />
        </>
      ) : !open ? (
        <button type="button" onClick={() => setOpen(true)}>
          Set up device authorization
        </button>
      ) : (
        <form
          class="stack-form qr-start-form"
          onSubmit={(event) => {
            event.preventDefault()
            void matrixOAuthQrGrant.start(accountId, presentation)
          }}
        >
          <label>
            Matrix account to authorize
            <select
              value={accountId}
              disabled={matrixOAuthQrGrant.operation.value === 'starting'}
              onChange={(event) => setAccountId(event.currentTarget.value)}
            >
              {eligible.map((account) => (
                <option key={account.account_id} value={account.account_id}>
                  {account.user_id}
                </option>
              ))}
            </select>
          </label>
          <ExpectedAccountGuidance account={expectedAccount} />
          <fieldset>
            <legend>How will this browser exchange the QR code?</legend>
            <label>
              <input
                type="radio"
                name="grant-qr-presentation"
                value="scan"
                checked={presentation === 'scan'}
                onChange={() => setPresentation('scan')}
              />
              Scan the QR code shown by the new Matrix client
            </label>
            <label>
              <input
                type="radio"
                name="grant-qr-presentation"
                value="display"
                checked={presentation === 'display'}
                onChange={() => setPresentation('display')}
              />
              Show a QR code for the new Matrix client to scan
            </label>
          </fieldset>
          <button
            type="submit"
            disabled={
              accountId === '' ||
              matrixOAuthQrGrant.operation.value === 'starting'
            }
          >
            {matrixOAuthQrGrant.operation.value === 'starting'
              ? 'Preparing…'
              : 'Start device authorization'}
          </button>
          <ErrorBanner error={matrixOAuthQrGrant.error} />
        </form>
      )}
    </section>
  )
}

function ExpectedAccountGuidance({ account }: { account?: Account }) {
  if (account === undefined) {
    return (
      <p class="expected-account-guidance" role="status">
        The account for this recovered flow is no longer available. Cancel it
        and start again with an active, verified account.
      </p>
    )
  }
  return (
    <p class="expected-account-guidance">
      In the new client, sign in as <strong>{account.user_id}</strong>. Do not
      approve the flow if the authorization service shows a different Matrix
      account.
    </p>
  )
}
