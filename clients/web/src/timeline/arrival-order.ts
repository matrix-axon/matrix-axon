/**
 * Arrival order is the key a Matrix read receipt is interpreted in, and it is
 * not the display order (ADR 0089). Every "which event may this client
 * acknowledge" decision picks a maximum in this key, and there are now three of
 * them — the room's own target, the room view's extension over read threads, and
 * `ThreadPanel`'s pick — so the comparison lives in one place. A change to
 * tie-breaking is then one edit rather than three.
 */
export interface HasArrivalOrder {
  arrival_order: number
}

/**
 * The event with the greatest `arrival_order`, or `null` for an empty input.
 *
 * Ties keep the **last** one seen, matching the TUI's `read_targets_for`
 * (`clients/tui/src/app/read_markers.rs`), which documents the same choice:
 * bridges stamp bursts within a single millisecond, so equal keys are common
 * rather than exotic, and the later row is the one a page renders last.
 */
export function maxByArrivalOrder<T extends HasArrivalOrder>(
  events: Iterable<T>,
): T | null {
  let best: T | null = null
  for (const event of events) {
    if (best === null || event.arrival_order >= best.arrival_order) {
      best = event
    }
  }
  return best
}

/**
 * Whether a view may claim read state at all: it is not parked on an anchor,
 * its slice reaches the live end, and that slice has actually loaded.
 *
 * The room stream and a thread panel each answer this for themselves — one from
 * `?event=` and the room timeline, the other from its highlight and the thread
 * timeline — and they had derived it independently, so a future condition could
 * be added to one and missed in the other (review, non-blocking). The inputs
 * differ; the rule does not.
 *
 * `atEnd` starts `true` on a cold store, which is why `loading` is a separate
 * term rather than an implication of it.
 */
export function viewMayClaimReadState(view: {
  anchoredTo: string | null
  atEnd: boolean
  loading: boolean
}): boolean {
  return view.anchoredTo === null && view.atEnd && !view.loading
}

/**
 * The arrival-max event strictly below `ceiling`, or the arrival-max overall
 * when there is no ceiling.
 *
 * Both receipt call sites bound their pick this way — the room view against the
 * first unread foreign reply above its own target, the thread panel against the
 * bound the room view hands it — and they had written the comparison
 * separately. That is the duplication `viewMayClaimReadState` was extracted to
 * end, one rule lower down (review).
 */
export function maxByArrivalOrderBelow<T extends HasArrivalOrder>(
  events: Iterable<T>,
  ceiling: number | null,
): T | null {
  return maxByArrivalOrder(
    ceiling === null
      ? events
      : [...events].filter((event) => event.arrival_order < ceiling),
  )
}
