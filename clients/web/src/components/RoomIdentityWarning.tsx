import { useLayoutEffect, useRef, useState } from 'preact/hooks'
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
  const actionList = useRef<HTMLUListElement>(null)
  const nextActionIndex = useRef<number | null>(null)
  const latestWarnings = useRef(warnings)
  const { containerRef } = useModalFocus<HTMLDivElement>({ restoreTo })
  const inFlight = useRef(false)
  const [busy, setBusy] = useState(false)
  const [feedback, setFeedback] = useState<{
    error: boolean
    message: string
  } | null>(null)
  const [acknowledged, setAcknowledged] = useState<{
    snapshot: readonly IdentityChange[]
    warnings: readonly IdentityChange[]
  } | null>(null)

  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        onClose()
      },
    },
    { whileTyping: true, capture: true },
  )

  useLayoutEffect(() => {
    latestWarnings.current = warnings
    const index = nextActionIndex.current
    if (busy || index === null) return
    nextActionIndex.current = null
    // Removing the row shifts its successor into its place. If the last
    // row was removed, focus the preceding one or Close for an empty list.
    const actions = actionList.current?.querySelectorAll('button')
    const target =
      actions?.[Math.min(index, actions.length - 1)] ?? closeButton.current
    target?.focus()
  }, [acknowledged, busy, warnings])

  const acknowledge = async (warning: IdentityChange, index: number) => {
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
        nextActionIndex.current = index
        const snapshot = latestWarnings.current
        setAcknowledged((current) => ({
          snapshot,
          warnings: [
            ...(current?.snapshot === snapshot ? current.warnings : []),
            warning,
          ],
        }))
        setFeedback({
          error: false,
          message: withdraw
            ? 'Verification withdrawn.'
            : 'New identity accepted.',
        })
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

  // Optimistic removal applies to the snapshot on screen at success.
  // Every new server snapshot decides which warnings need review again.
  const remaining =
    acknowledged?.snapshot === warnings
      ? warnings.filter(
          (warning) =>
            !acknowledged.warnings.some(
              (resolved) =>
                resolved.user_id === warning.user_id &&
                resolved.kind === warning.kind &&
                resolved.fingerprint === warning.fingerprint,
            ),
        )
      : warnings

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
            <ul ref={actionList}>
              {remaining.map((warning, index) => {
                const withdraw = warning.kind === 'verification_violation'
                const action = withdraw
                  ? 'Withdraw verification'
                  : 'Accept new identity'
                return (
                  <li key={warning.user_id}>
                    <strong>{warning.user_id}</strong>
                    <p>
                      {withdraw
                        ? 'Their verified identity changed. You can verify them again or withdraw your previous verification.'
                        : 'Their identity differs from the one Axon first saw.'}
                    </p>
                    <button
                      type="button"
                      aria-disabled={busy}
                      aria-label={`${action} for ${warning.user_id}`}
                      onClick={() => void acknowledge(warning, index)}
                    >
                      {action}
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
