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
  liveThreadRoots: ReadonlySet<string>,
): boolean {
  // A redacted thread root that still has replies is the only way into them
  // from the room, so the preference yields: hiding it would orphan the thread.
  return hideRedacted && event.redacted && !liveThreadRoots.has(event.event_id)
}

/**
 * The `liveThreadRoots` argument for a caller with no thread roots to except,
 * such as a thread's own replies (a reply cannot be a root). Named so that
 * passing nothing is a visible decision rather than an omission.
 */
export const NO_THREAD_ROOTS: ReadonlySet<string> = new Set()
