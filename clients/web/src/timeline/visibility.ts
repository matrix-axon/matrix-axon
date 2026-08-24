/**
 * Whether the hide-redacted-events setting hides this event.
 *
 * Three places asked this independently — the room's visibility predicate, the
 * room-receipt candidate set, and the thread panel's shown replies — with the
 * same expression written out each time. A mutation aimed at one of them landed
 * on another during this PR's own testing, because the text was identical
 * (review).
 */
export function hiddenByRedaction(
  event: { redacted: boolean },
  hideRedacted: boolean,
): boolean {
  return hideRedacted && event.redacted
}
