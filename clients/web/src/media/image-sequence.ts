import { eventMedia, isImageMedia } from './event-media'
import type { ParsedMedia } from './parse-media'
import type { TimelineEvent } from '../stores/timeline'

/**
 * One viewable image, in timeline order. The lightbox pages across these
 * (ADR 0081).
 */
export interface LightboxItem {
  eventId: string
  /** The source event, used by contextual viewer actions. */
  event: TimelineEvent
  /** `origin_ts`, kept so a cursor whose event disappears can find its
   *  nearest surviving neighbour rather than closing. */
  ts: number
  accountId: string
  media: ParsedMedia
}

/**
 * The images in a rendered event list, oldest first.
 *
 * Callers must pass the list they actually *render* — `RoomPage`'s `visible`,
 * not the raw store slice. The slice carries thread replies and hidden state
 * events the timeline refuses to draw; an image paged in from one of those
 * would strand the viewer on a photo with no row to return to on close.
 *
 * Still-uploading echoes are excluded on purpose. Their bytes live in an
 * object url the composer owns and revokes on reconcile, so a viewer holding
 * one could have it pulled out from under it mid-view; and there is nothing
 * to fetch through the media proxy until the upload lands.
 */
export function imageSequence(
  events: readonly TimelineEvent[],
): LightboxItem[] {
  const items: LightboxItem[] = []
  for (const event of events) {
    const resolved = eventMedia(event)
    if (resolved === null || resolved.pending) {
      continue
    }
    const { media } = resolved
    if (!isImageMedia(media) || media.url === null) {
      continue
    }
    items.push({
      eventId: event.event_id,
      event,
      ts: event.origin_ts,
      accountId: event.account_id,
      media,
    })
  }
  return items
}
