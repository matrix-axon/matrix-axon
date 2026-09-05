import { useServices } from '../services'
import {
  flowTitle,
  VERIFICATION_ENDED_BY_SERVER,
  type VerificationFlow,
} from '../stores/verification'
import { BodyPortal } from './BodyPortal'
import { useModalFocus } from './use-modal-focus'
import { useShortcuts } from '../shortcuts'

function targetLine(flow: VerificationFlow): string {
  const user = flow.userId !== '' ? flow.userId : null
  const device =
    flow.deviceId !== null && flow.deviceId !== '' ? flow.deviceId : null
  if (user !== null && device !== null) {
    return `${user} (device ${device})`
  }
  return user ?? device ?? ''
}

export function SasModal() {
  const { verification } = useServices()
  const flow = verification.openFlow.value
  const { containerRef } = useModalFocus<HTMLDivElement>()

  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        if (flow === null) {
          return
        }
        if (flow.stage === 'done' || flow.stage === 'ended') {
          verification.dismissTerminal(verification.openKey.value ?? '')
          return
        }
        verification.closeModal()
      },
    },
    { whileTyping: true, capture: true },
  )

  if (flow === null) {
    return null
  }

  const key = verification.openKey.value ?? ''
  const canConfirm =
    flow.stage === 'compare' && flow.emoji !== null && flow.emoji.length === 7
  const live =
    flow.stage === 'starting' ||
    flow.stage === 'waiting' ||
    flow.stage === 'compare' ||
    flow.stage === 'confirming'

  const closeOrPark = () => {
    if (live) {
      verification.closeModal()
      return
    }
    verification.dismissTerminal(key)
  }

  return (
    <BodyPortal>
      <div
        ref={containerRef}
        class="overlay"
        role="dialog"
        aria-modal="true"
        aria-labelledby="sas-modal-title"
      >
        <div class="overlay-panel sas-modal">
          <div class="overlay-head">
            <h2 id="sas-modal-title">{flowTitle(flow)}</h2>
            <button type="button" class="ghost" onClick={closeOrPark}>
              Close
            </button>
          </div>
          {targetLine(flow) !== '' && (
            <p class="muted sas-target">{targetLine(flow)}</p>
          )}
          {flow.error !== null && (
            <p class="error" role="alert">
              {flow.error}
            </p>
          )}
          {flow.stage === 'starting' && (
            <p>
              Starting verification of {targetLine(flow) || 'the other device'}…
            </p>
          )}
          {flow.stage === 'waiting' && <p>Waiting for the other device…</p>}
          {flow.stage === 'compare' && (
            <>
              <p>Compare these emoji with the other device. Do they match?</p>
              {flow.emoji !== null && (
                <ol class="sas-emoji">
                  {flow.emoji.map((item, index) => (
                    <li key={`${index}:${item.symbol}`}>
                      <span class="sas-emoji-symbol" aria-hidden="true">
                        {item.symbol}
                      </span>{' '}
                      {item.description}
                    </li>
                  ))}
                </ol>
              )}
              {flow.decimals !== null && (
                <p class="muted">
                  Decimal fallback: {flow.decimals[0]} - {flow.decimals[1]} -{' '}
                  {flow.decimals[2]}
                </p>
              )}
            </>
          )}
          {flow.stage === 'confirming' && (
            <p>You confirmed. Waiting for the other device to confirm…</p>
          )}
          {flow.stage === 'done' && <p>Verification complete.</p>}
          {flow.stage === 'ended' && (
            <p>
              {flow.cancelReason !== null && flow.cancelReason !== ''
                ? flow.cancelReason
                : VERIFICATION_ENDED_BY_SERVER}
            </p>
          )}
          <div class="dialog-actions">
            {flow.stage === 'waiting' && flow.flowId !== null && (
              <button
                type="button"
                class="ghost"
                onClick={() => void verification.refresh(flow.accountId)}
              >
                Refresh
              </button>
            )}
            {live && (
              <button
                type="button"
                onClick={() => void verification.requestCancel(key)}
              >
                {flow.stage === 'compare' ? "They don't match" : 'Cancel'}
              </button>
            )}
            {flow.stage === 'compare' && (
              <button
                type="button"
                disabled={!canConfirm}
                onClick={() => {
                  if (flow.flowId !== null) {
                    void verification.confirm(flow.accountId, flow.flowId)
                  }
                }}
              >
                They match
              </button>
            )}
            {(flow.stage === 'done' || flow.stage === 'ended') && (
              <button
                type="button"
                onClick={() => verification.dismissTerminal(key)}
              >
                OK
              </button>
            )}
          </div>
        </div>
      </div>
    </BodyPortal>
  )
}
