/**
 * Whether the hide-redacted-events setting hides this event.
 *
 * Three places asked this independently — the room's visibility predicate, the
 * room-receipt candidate set, and the thread panel's shown replies — with the
 * same expression written out each time. A mutation aimed at one of them landed
 * on another during this PR's own testing, because the text was identical
 * (review).
 *
 * The preference has one exception: a redacted event that is the root of a
 * thread with non-redacted replies is never hidden.
 */
export function hiddenByRedaction(
  event: { redacted: boolean; event_id: string },
  hideRedacted: boolean,
  liveThreadRoots?: ReadonlyMap<string, { reply_count: number }>,
): boolean {
  if (!hideRedacted || !event.redacted) {
    return false
  }
  // A redacted thread root that still has replies is the only way into them
  // from the room, so the preference yields: hiding it would orphan the thread.
  // `reply_count` already excludes redacted members, so a positive count means
  // there is something left to read.
  return (liveThreadRoots?.get(event.event_id)?.reply_count ?? 0) <= 0
}
