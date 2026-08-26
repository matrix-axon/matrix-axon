import { sameLocalDay } from '../calendar-day'
import { eventMedia, isImageMedia } from '../media/event-media'
import { inReplyToId, type TimelineEvent } from '../stores/timeline'

/**
 * A run of images has to be close together in time to read as one gesture.
 * A minute is generous for a slow upload of several photos and still far
 * short of "two people posting pictures in the same conversation".
 */
export const GALLERY_WINDOW_MS = 60_000

/** Below this, a run renders as ordinary rows — two images is the smallest
 *  thing that reads as a group rather than a coincidence. */
export const GALLERY_MIN = 2

/**
 * Above this, a run splits into several galleries. Bounds both the grid's
 * height and the number of objects one row can pull; see ADR 0081 on why the
 * *fetch* is gated by size rather than by this cap.
 */
export const GALLERY_MAX = 12

export type TimelineRow =
  | { kind: 'event'; key: string; event: TimelineEvent }
  | { kind: 'gallery'; key: string; events: TimelineEvent[] }

export interface GroupMediaRunsOptions {
  windowMs?: number
  min?: number
  max?: number
  /**
   * Force an event to render as an ordinary row. Used for anything that needs
   * the full `MessageEventRow` apparatus — the highlighted jump target, an
   * event carrying a thread summary — without this function having to know
   * what those things are.
   */
  breaks?: (event: TimelineEvent) => boolean
}

/**
 * Whether this event can sit in a gallery grid at all — before any question
 * of which run it might join.
 *
 * The exclusions are cases where the grid would lose something the full row
 * provides. Reactions bring chips and a picker, a reply brings its context
 * block; rendering either per cell would drag `MessageEventRow`'s whole
 * apparatus into the grid, and dropping them would silently hide information.
 * A failed send needs its retry affordance, which the full row already gives.
 *
 * An **edit does not break a run**. It was originally lumped in with
 * reactions, but the two are not alike: editing an image's caption changes
 * content the grid already shows beneath the cells, not structure, and losing
 * the whole gallery because one caption was corrected is a jarring price for
 * a marker. `origin_ts` is unaffected by an edit — the edit time lives in
 * `latest_edit_ts` — so the run's timing is unchanged too. The cost is that
 * the "(edited)" marker is not visible in grid form; the expander still
 * reaches it.
 */
function groupable(
  event: TimelineEvent,
  breaks: ((event: TimelineEvent) => boolean) | undefined,
): boolean {
  if (breaks?.(event) === true) {
    return false
  }
  if (
    event.redacted ||
    (event.state_key !== null && event.state_key !== undefined) ||
    event.localEcho?.status === 'failed' ||
    inReplyToId(event) !== null
  ) {
    return false
  }
  // A per-emoji tally keyed by emoji, not a list — same shape `MessageEventRow`
  // reads. `length` would be `undefined > 0`, i.e. silently never breaking.
  const reactions = event.reactions
  if (
    reactions !== null &&
    reactions !== undefined &&
    Object.keys(reactions).length > 0
  ) {
    return false
  }
  const resolved = eventMedia(event)
  return resolved !== null && isImageMedia(resolved.media)
}

/**
 * Group adjacent images from one sender into gallery rows, leaving everything
 * else as ordinary rows.
 *
 * Matrix has no album (ADR 0081), so a multi-image send arrives as N
 * unrelated `m.image` events and the grouping is inferred here, at render
 * time. Being a pure function over the array — not a component, not store
 * state — is what makes every break reason testable in isolation.
 *
 * "No intervening events of any kind" falls out rather than being checked:
 * the walk is linear, and anything that fails `groupable` flushes the run in
 * progress before emitting itself.
 *
 * Read receipts deliberately do **not** break a run. They change on every
 * ephemeral update, so re-grouping on them would relayout the timeline with
 * no user action behind it — precisely the hazard ADR 0076 exists to prevent.
 */
export function groupMediaRuns(
  visible: readonly TimelineEvent[],
  options: GroupMediaRunsOptions = {},
): TimelineRow[] {
  const {
    windowMs = GALLERY_WINDOW_MS,
    min = GALLERY_MIN,
    max = GALLERY_MAX,
    breaks,
  } = options

  const rows: TimelineRow[] = []
  let run: TimelineEvent[] = []

  const flush = () => {
    if (run.length >= min) {
      rows.push({
        kind: 'gallery',
        // Keyed on the first event, which is stable under a back-pagination
        // prepend — an index would re-pair every row by position (WCR-01).
        key: `gallery:${run[0].event_id}`,
        events: run,
      })
    } else {
      for (const event of run) {
        rows.push({ kind: 'event', key: event.event_id, event })
      }
    }
    run = []
  }

  for (const event of visible) {
    if (!groupable(event, breaks)) {
      flush()
      rows.push({ kind: 'event', key: event.event_id, event })
      continue
    }
    const previous = run[run.length - 1]
    const joins =
      previous !== undefined &&
      event.sender === previous.sender &&
      // Absolute, so the function does not quietly assume ascending order. A
      // signed comparison passes for *any* negative delta, so a
      // newest-first list would group images hours apart — which is exactly
      // what the e2e fixture does.
      Math.abs(event.origin_ts - previous.origin_ts) <= windowMs &&
      // Local midnight, so a run cannot straddle a day separator.
      sameLocalDay(previous.origin_ts, event.origin_ts) &&
      run.length < max
    if (!joins) {
      flush()
    }
    run.push(event)
  }
  flush()

  return rows
}

/** The timestamp a row sorts and separates by — its first event's. */
export function rowTs(row: TimelineRow): number {
  return row.kind === 'event' ? row.event.origin_ts : row.events[0].origin_ts
}

/** The first event of a row, for anchoring and `data-event-id`. */
export function rowLeadEvent(row: TimelineRow): TimelineEvent {
  return row.kind === 'event' ? row.event : row.events[0]
}

/**
 * The position of an event within its gallery run, or `null` when it is not in
 * one. Feeds the lightbox's run-aware counter (ADR 0081) — paging itself stays
 * global, so a mis-grouped run can never become a navigation cage.
 */
export function runPosition(
  rows: readonly TimelineRow[],
  eventId: string,
): { index: number; total: number } | null {
  for (const row of rows) {
    if (row.kind !== 'gallery') {
      continue
    }
    const index = row.events.findIndex((event) => event.event_id === eventId)
    if (index !== -1) {
      return { index, total: row.events.length }
    }
  }
  return null
}
