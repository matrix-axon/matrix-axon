import { useEffect } from 'preact/hooks'
import { useServices } from '../services'
import { emptyRoomPreview, roomDetailsKey } from '../stores/room-details'
import { DetailRow, shownProps } from './DetailRow'
import {
  discoveredPreview,
  type DiscoveredRoomDto,
} from './room-details-format'

/**
 * Preview one unjoined room while the calling prompt is mounted and `enabled`
 * (decision 8): the store reads its `/discovery`, keeps it fresh under
 * decision 2, and aborts the read when the last prompt for the room closes.
 * The seed a prompt shows until then is its own (`RoomPreview`'s `seed`).
 */
export function useRoomPreview(
  accountId: string,
  roomId: string,
  enabled: boolean,
): void {
  const { roomDetails } = useServices()
  useEffect(() => {
    if (!enabled) return
    const target = { accountId, roomId }
    roomDetails.showPreview(target)
    return () => roomDetails.closePreview(target)
  }, [roomDetails, accountId, roomId, enabled])
}

/**
 * The summary rows for one unjoined room (decisions 3 and 8): the store's
 * `/discovery` answer when it has one for this room, else `seed`. The caller
 * opens and closes the preview; this only renders it, along with a failed
 * read and its Retry. `requireAvailable` hides it entirely until the room is
 * eligible and its summary available, for a prompt that has nothing else to
 * say about the room (a failure with nothing to show hides it too).
 */
export function RoomPreview({
  accountId,
  roomId,
  seed,
  requireAvailable = false,
}: {
  accountId: string
  roomId: string
  seed: DiscoveredRoomDto | null
  requireAvailable?: boolean
}) {
  const { roomDetails } = useServices()
  const key = roomDetailsKey(accountId, roomId)
  const stored = roomDetails.preview.value
  const current = stored.key === key ? stored : emptyRoomPreview(key)
  const answer = current.discovery.value
  const discovered = answer?.discovered ?? seed
  if (
    requireAvailable &&
    (answer === null ||
      !answer.eligible ||
      discovered === null ||
      discovered.status !== 'available')
  ) {
    return null
  }
  const shown = discoveredPreview(discovered)
  // A failed read is reported, never left looking like one in flight. Any
  // summary already on hand (the seed, or an earlier answer) stays, marked as
  // not refreshed.
  const error = current.discovery.error
  return (
    <section class="room-join-preview" aria-label="Room preview">
      {shown.rows === null ? (
        error === null && <p class="muted">{shown.label}</p>
      ) : (
        <>
          <dl class="detail-list">
            {shown.rows.map((row) => (
              <DetailRow
                key={row.label}
                label={row.label}
                {...shownProps(row.shown)}
                code={row.label === 'Canonical alias'}
              />
            ))}
          </dl>
          <p class="room-join-preview-note muted">
            {error === null ? shown.note : `${shown.note} · couldn't refresh`}
          </p>
        </>
      )}
      {error !== null && (
        <div class="room-join-preview-error" role="alert">
          <p class="error">Couldn't look up this room: {error}</p>
          <button
            type="button"
            class="ghost"
            onClick={() => roomDetails.retryPreview()}
          >
            Retry
          </button>
        </div>
      )}
    </section>
  )
}
