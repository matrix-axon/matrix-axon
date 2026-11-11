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
 * decision 2, and aborts the read when the prompt closes. `seed` is read once,
 * when the preview opens: a later copy of the same summary changes nothing
 * the read will not.
 */
export function useRoomPreview(
  accountId: string,
  roomId: string,
  seed: DiscoveredRoomDto | null,
  enabled: boolean,
): void {
  const { roomDetails } = useServices()
  useEffect(() => {
    if (!enabled) return
    const target = { accountId, roomId, seed }
    roomDetails.showPreview(target)
    return () => roomDetails.closePreview(target)
    // `seed` only seeds: a new object for the same room must not abort and
    // repeat the read.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [roomDetails, accountId, roomId, enabled])
}

/**
 * The summary rows for one unjoined room (decisions 3 and 8): the store's
 * `/discovery` answer when it has one for this room, else `seed`. The caller
 * opens and closes the preview; this only renders it. `requireAvailable`
 * hides it entirely until the room is eligible and its summary available,
 * for a prompt that has nothing else to say about the room.
 */
export function RoomPreview({
  accountId,
  roomId,
  seed,
  formatTime,
  requireAvailable = false,
}: {
  accountId: string
  roomId: string
  seed: DiscoveredRoomDto | null
  formatTime: (ts: number) => string
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
  const shown = discoveredPreview(discovered, formatTime)
  return (
    <section class="room-preview" aria-label="Room preview">
      {shown.rows === null ? (
        <p class="muted">{shown.label}</p>
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
          <p class="room-preview-note muted">{shown.note}</p>
        </>
      )}
    </section>
  )
}
