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
  /**
   * The redaction the server folded onto this event, and where it arrived
   * (#1273). Both or neither: a row cached before the server reported the
   * arrival order contributes no redaction position.
   */
  redaction_event_id?: string | null
  redaction_arrival_order?: number | null
}

/**
 * The redactions folded onto `events`, each as a receipt position of its own
 * (#1273, ADR 0089 amendment).
 *
 * Timeline reads strip bare `m.room.redaction` events and report each one on
 * the event it redacted, so a redaction is never an event of its own here. That
 * is harmless until a room rule set to notify on every event counts the
 * redaction: it is then the room's newest notification, and a receipt that
 * stops at the redacted message leaves it counted forever.
 *
 * These are *not* displayed events. A redaction's content is visible on its
 * target, but its arrival position is its own, usually well above the target's,
 * and naming it acknowledges everything that arrived in between. So
 * `computeRoomReceipt` bounds them like the read-thread extension rather than
 * treating them as part of `displayed` (review).
 */
export function foldedRedactions(
  events: readonly ReceiptCandidate[],
): ReceiptCandidate[] {
  const positions: ReceiptCandidate[] = []
  for (const event of events) {
    const redactionId = event.redaction_event_id
    const redactionOrder = event.redaction_arrival_order
    if (typeof redactionId === 'string' && typeof redactionOrder === 'number') {
      positions.push({
        event_id: redactionId,
        arrival_order: redactionOrder,
        threadRoot: event.threadRoot,
      })
    }
  }
  return positions
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

/**
 * A position a read receipt may name: an event id and where it arrived. Only a
 * position, not an event: a folded redaction has no row of its own (review).
 */
export type ReceiptPosition = Pick<
  ReceiptCandidate,
  'event_id' | 'arrival_order'
>

export interface RoomReceipt {
  /** Exclusive arrival bound a thread panel may name up to. */
  blocker: number | null
  /**
   * The arrival-max position this view may name, or `null`: an event it holds,
   * or a redaction folded onto a displayed one, which has no event of its own.
   */
  target: ReceiptPosition | null
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
 * The base is the arrival-max of `displayed`, the events this view actually
 * showed. Read thread replies and folded redactions above it are extensions.
 * Nothing above the main timeline's own target is claimable while anything in
 * the room is unread — or merely unknown. The bound below can only see replies
 * the client has loaded, while thread summaries know about replies it has not,
 * so both signals are consulted and either one shuts the extension.
 */
export function computeRoomReceipt(input: RoomReceiptInput): RoomReceipt {
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
  const isRead = (event: ReceiptCandidate): boolean =>
    event.threadRoot !== null &&
    (readThrough.get(event.threadRoot) ?? -1) >= event.arrival_order

  const above: readonly ReceiptCandidate[] = input.loaded.filter(
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
  // A folded redaction above the base is an extension too, under the same
  // bound: everything that arrived between the base and the redaction is
  // acknowledged with it, so it is reachable only once nothing in the room is
  // unread or unknown, and only below the first unread foreign reply (#1273).
  const redaction = maxByArrivalOrderBelow(
    foldedRedactions(input.displayed).filter(
      (position) => position.arrival_order > baseOrder,
    ),
    blocker,
  )
  const target = maxByArrivalOrder(
    [base, claimable, redaction].filter(
      (position): position is ReceiptCandidate => position !== null,
    ),
  )
  return { blocker, target }
}
