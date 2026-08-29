import { useServices } from '../services'
import {
  flowKey,
  flowStageLabel,
  flowTitle,
  type VerificationFlow,
} from '../stores/verification'
import { BodyPortal } from './BodyPortal'
import { useModalFocus } from './use-modal-focus'
import { useShortcuts } from '../shortcuts'

function FlowRow({
  flow,
  showAccount,
  onClose,
}: {
  flow: VerificationFlow
  showAccount: boolean
  onClose: () => void
}) {
  const { verification } = useServices()
  const key = flowKey(flow)
  const done = flow.stage === 'done'
  return (
    <li class="verification-inbox-row">
      <button
        type="button"
        onClick={() => {
          verification.open(key)
          onClose()
        }}
      >
        <span>{flowTitle(flow)}</span>
        {showAccount && (
          <span class="muted">
            {' '}
            {flow.userId !== '' ? flow.userId : flow.accountId}
          </span>
        )}
        <span class="badge">{flowStageLabel(flow)}</span>
      </button>
      {done ? (
        <button
          type="button"
          class="ghost"
          onClick={() => verification.dismissTerminal(key)}
        >
          Dismiss
        </button>
      ) : (
        <button
          type="button"
          class="ghost"
          onClick={() => void verification.requestCancel(key)}
        >
          Decline
        </button>
      )}
    </li>
  )
}

export function VerificationInboxPanel({ onClose }: { onClose: () => void }) {
  const { verification, accounts } = useServices()
  const { containerRef } = useModalFocus<HTMLDivElement>()
  const inbox = verification.inbox.value
  const showAccount =
    accounts.accounts.value.filter((account) => account.state === 'active')
      .length > 1

  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        onClose()
      },
    },
    { whileTyping: true, capture: true },
  )

  return (
    <BodyPortal>
      <div
        ref={containerRef}
        class="overlay unread-threads-overlay"
        role="dialog"
        aria-modal="true"
        aria-labelledby="verification-inbox-title"
      >
        <div class="overlay-panel unread-threads-panel">
          <div class="overlay-head">
            <h2 id="verification-inbox-title">Device verification</h2>
            <button type="button" class="ghost" onClick={onClose}>
              Close
            </button>
          </div>
          {inbox.length === 0 ? (
            <p class="muted">No pending verification requests.</p>
          ) : (
            <ul class="verification-inbox-list">
              {inbox.map((flow) => (
                <FlowRow
                  key={flowKey(flow)}
                  flow={flow}
                  showAccount={showAccount}
                  onClose={onClose}
                />
              ))}
            </ul>
          )}
        </div>
      </div>
    </BodyPortal>
  )
}
