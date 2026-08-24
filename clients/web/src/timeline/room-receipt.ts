import type { ThreadReadMarker, ReadMarker } from '../stores/device-state'
import type { ThreadSummaryDto } from '../stores/threads'
import { maxByArrivalOrder, maxByArrivalOrderBelow } from './arrival-order'

/**
 * How far this client may acknowledge the room, in arrival order (ADR 0096).
 *
 * Extracted from `RoomPage` so the rule can be tested on its own: it is the part
 * of this feature where being wrong sends an irreversible claim to the
 * homeserver, and reaching it only through a rendered page made each case an
 * exercise in fixture plumbing (review).
 */
export interface ReceiptCandidate {
  event_id: string
  arrival_order: number
  /** The thread this event belongs to, or `null` for the main timeline. */
  threadRoot: string | null
}

export interface RoomReceiptInput {
  /** Main-timeline events this view displayed and may claim. */
  displayed: readonly ReceiptCandidate[]
  /**
   * Every event the client holds for the room, displayed or not — thread
   * replies included, since those are what the bound is made of.
   */
  loaded: readonly ReceiptCandidate[]
  /** The thread whose panel is open; its members are the panel's to claim. */
  openThread: string | null
  /** Thread summaries for the room, with each thread's marker. */
  threads: readonly {
    summary: ThreadSummaryDto
    marker: ThreadReadMarker | null
  }[]
  /** Fallback read position for a thread with no marker of its own. */
  roomMarker: ReadMarker | null
  /** False while any store this depends on is still loading or errored. */
  storesLoaded: boolean
  /** Set when a thread is already known unread from live events. */
  knownUnreadCutoff: number | null
}

export interface RoomReceipt<T extends ReceiptCandidate> {
  /** Exclusive arrival bound a thread panel may name up to. */
  blocker: number | null
  /** The arrival-max event this view may name, or `null`. */
  target: T | null
}

/**
 * Whether a thread summary shows unread replies against a read position.
 *
 * The one place this comparison is written. `reconcileSummary` asks it to decide
 * what to *display* and treats an absent position as "say nothing"; the receipt
 * asks it to decide what to *claim* and treats the same absence as unread. The
 * two defaults are deliberate and were previously two separate expressions that
 * could drift (review) — so the difference is now a parameter, not a fork.
 */
export function summaryLooksUnread(
  summary: ThreadSummaryDto,
  markerTs: number | null,
  unknownCounts: 'unread' | 'silent',
): boolean {
  const latestTs = summary.latest_reply_ts ?? null
  if (latestTs === null) {
    return false
  }
  if (markerTs === null) {
    return unknownCounts === 'unread'
  }
  return latestTs > markerTs
}

/**
 * Pick the receipt bound and target for a room.
 *
 * Nothing above the main timeline's own target is claimable while anything in
 * the room is unread — or merely unknown. The bound below can only see replies
 * the client has loaded, while thread summaries know about replies it has not,
 * so both signals are consulted and either one shuts the extension.
 */
export function computeRoomReceipt<T extends ReceiptCandidate>(
  input: Omit<RoomReceiptInput, 'displayed' | 'loaded'> & {
    displayed: readonly T[]
    loaded: readonly T[]
  },
): RoomReceipt<T> {
  const base = maxByArrivalOrder(input.displayed)
  const baseOrder = base?.arrival_order ?? -1

  const anythingUnread =
    !input.storesLoaded ||
    input.knownUnreadCutoff !== null ||
    input.threads.some(({ summary, marker }) =>
      summaryLooksUnread(
        summary,
        marker?.originTs ?? input.roomMarker?.originTs ?? null,
        'unread',
      ),
    )
  if (anythingUnread) {
    return { blocker: baseOrder + 1, target: base }
  }

  /**
   * How far each thread has been read, in arrival order. `arrivalThrough` is
   * what the panel recorded displaying; the fallback resolves the marker's event
   * in the loaded set and is strictly weaker — it understates a thread whose
   * display-last event is not its arrival-max, and answers nothing once that
   * event pages out.
   */
  const needsFallback = input.threads.some(
    ({ marker }) => marker !== null && marker.arrivalThrough === null,
  )
  // Built only for a marker written before `arrivalThrough` existed. This is the
  // steady-state branch — it reruns on every new message — and the map is a full
  // pass over the slice serving, usually, zero markers (review).
  const arrivalOf = needsFallback
    ? new Map(
        input.loaded.map((event) => [event.event_id, event.arrival_order]),
      )
    : null
  const readThrough = new Map<string, number>()
  for (const { summary, marker } of input.threads) {
    readThrough.set(
      summary.root_event_id,
      marker === null
        ? -1
        : (marker.arrivalThrough ?? arrivalOf?.get(marker.eventId) ?? -1),
    )
  }
  const isRead = (event: T): boolean =>
    event.threadRoot !== null &&
    (readThrough.get(event.threadRoot) ?? -1) >= event.arrival_order

  const above: readonly T[] = input.loaded.filter(
    (event) =>
      event.arrival_order > baseOrder &&
      event.threadRoot !== null &&
      event.threadRoot !== input.openThread,
  )
  const blocker = above
    .filter((event) => !isRead(event))
    .reduce<number | null>(
      (lowest, event) =>
        lowest === null || event.arrival_order < lowest
          ? event.arrival_order
          : lowest,
      null,
    )
  const claimable = maxByArrivalOrderBelow(above.filter(isRead), blocker)
  const target = maxByArrivalOrder(
    [base, claimable].filter((event): event is T => event !== null),
  )
  return { blocker, target }
}
