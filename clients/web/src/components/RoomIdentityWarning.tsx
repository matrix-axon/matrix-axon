import { useRef, useState } from 'preact/hooks'
import { apiErrorMessage } from '../api/client'
import type { components } from '../api/schema'
import { useServices } from '../services'
import { useShortcuts } from '../shortcuts'
import { BodyPortal } from './BodyPortal'
import { useModalFocus } from './use-modal-focus'

type IdentityChange = components['schemas']['RoomIdentityChangeDto']

export function RoomIdentityWarning({
  count,
  onReview,
  announce = true,
}: {
  count: number
  onReview: (opener: HTMLButtonElement) => void
  announce?: boolean
}) {
  if (count === 0) return null
  return (
    <div class="trust-warning">
      <span role={announce ? 'status' : undefined}>
        {count === 1
          ? 'A room member’s identity changed.'
          : `${count} room members’ identities changed.`}
      </span>
      <button
        type="button"
        class="ghost"
        onClick={(event) => onReview(event.currentTarget)}
      >
        Review identity changes
      </button>
    </div>
  )
}

export function RoomIdentityReview({
  accountId,
  roomId,
  warnings,
  onClose,
  restoreTo,
}: {
  accountId: string
  roomId: string
  warnings: readonly IdentityChange[]
  onClose: () => void
  restoreTo: () => HTMLElement | null
}) {
  const { api, roomDetails } = useServices()
  const closeButton = useRef<HTMLButtonElement>(null)
  const { containerRef } = useModalFocus<HTMLDivElement>({ restoreTo })
  const inFlight = useRef(false)
  const [busy, setBusy] = useState(false)
  const [feedback, setFeedback] = useState<{
    error: boolean
    message: string
  } | null>(null)
  const [acknowledged, setAcknowledged] = useState<IdentityChange[]>([])

  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        onClose()
      },
    },
    { whileTyping: true, capture: true },
  )

  const acknowledge = async (warning: IdentityChange) => {
    if (inFlight.current) return
    const withdraw = warning.kind === 'verification_violation'
    inFlight.current = true
    setBusy(true)
    setFeedback(null)
    try {
      const { error, response } = await api.POST(
        withdraw
          ? '/v1/accounts/{account_id}/users/{user_id}/identity/withdraw_verification'
          : '/v1/accounts/{account_id}/users/{user_id}/identity/accept',
        {
          params: { path: { account_id: accountId, user_id: warning.user_id } },
          body: { fingerprint: warning.fingerprint },
        },
      )
      if (response.ok) {
        setAcknowledged((current) => [...current, warning])
        setFeedback({
          error: false,
          message: withdraw
            ? 'Verification withdrawn.'
            : 'New identity accepted.',
        })
        closeButton.current?.focus()
      } else {
        setFeedback({ error: true, message: apiErrorMessage(error) })
      }
      if (response.ok || response.status === 409 || response.status === 404) {
        roomDetails.refreshInfo(accountId, roomId)
      }
    } catch (cause) {
      setFeedback({
        error: true,
        message: cause instanceof Error ? cause.message : String(cause),
      })
    } finally {
      inFlight.current = false
      setBusy(false)
    }
  }

  const remaining = warnings.filter(
    (warning) =>
      !acknowledged.some(
        (resolved) =>
          resolved.user_id === warning.user_id &&
          resolved.kind === warning.kind &&
          resolved.fingerprint === warning.fingerprint,
      ),
  )

  return (
    <BodyPortal>
      <div
        ref={containerRef}
        class="overlay"
        role="dialog"
        aria-modal="true"
        aria-labelledby="room-identity-review-title"
      >
        <div class="overlay-panel room-identity-review">
          <div class="overlay-head">
            <h2 id="room-identity-review-title">Review identity changes</h2>
            <button
              ref={closeButton}
              type="button"
              class="ghost"
              onClick={onClose}
            >
              Close
            </button>
          </div>
          {feedback !== null && (
            <p
              role={feedback.error ? 'alert' : 'status'}
              class={feedback.error ? 'error' : undefined}
            >
              {feedback.message}
            </p>
          )}
          {remaining.length === 0 ? (
            <p>No identity changes to review.</p>
          ) : (
            <ul>
              {remaining.map((warning) => {
                const withdraw = warning.kind === 'verification_violation'
                return (
                  <li
                    key={JSON.stringify([
                      warning.user_id,
                      warning.kind,
                      warning.fingerprint,
                    ])}
                  >
                    <strong>{warning.user_id}</strong>
                    <p>
                      {withdraw
                        ? 'Their verified identity changed. You can verify them again or withdraw your previous verification.'
                        : 'Their identity differs from the one Axon first saw.'}
                    </p>
                    <button
                      type="button"
                      disabled={busy}
                      onClick={() => void acknowledge(warning)}
                    >
                      {withdraw
                        ? 'Withdraw verification'
                        : 'Accept new identity'}
                    </button>
                  </li>
                )
              })}
            </ul>
          )}
        </div>
      </div>
    </BodyPortal>
  )
}
