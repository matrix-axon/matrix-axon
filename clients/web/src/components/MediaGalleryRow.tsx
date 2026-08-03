import { Fragment } from 'preact'
import { useMemo, useRef, useState } from 'preact/hooks'
import { eventMedia } from '../media/event-media'
import { useMediaViewer } from '../media/media-viewer'
import type { MembersStore } from '../stores/members'
import type { TimelineEvent } from '../stores/timeline'
import { useGalleryExpansion } from '../timeline/gallery-expansion'
import { GALLERY_EAGER_BYTES, MediaGalleryCell } from './MediaGalleryCell'
import { ReadReceiptsSummary } from './MessageEventRow'
import type { ReadReceipt } from '../stores/ephemeral'
import { UserAvatar } from './UserAvatar'

/** Columns in the grid. Fixed, not `auto-fit` — see the ADR on why. */
export const GALLERY_COLUMNS = 3

/**
 * Every control a gallery cell can render that takes focus. A failed cell is
 * included deliberately: it is still one of the sender's images, and skipping
 * it would make the run's length disagree with what arrow keys can reach.
 */
const NAVIGABLE =
  '.gallery-cell-open, .gallery-cell-deferred, .gallery-cell-failed'

/**
 * A run of adjacent images from one sender, drawn as one grid (ADR 0081).
 *
 * **Must be a single `<li class="event-row">` in the timeline's `<ol>`.**
 * `captureAnchor` binary-searches those on the assumption that document order
 * is monotonic in vertical position; a gallery that rendered as anything else
 * would break scroll anchoring for every row below it.
 *
 * The grid inside is a real `<ul>` of `<li>`, so assistive technology gets
 * "list, 7 items" with no ARIA at all.
 */
export function MediaGalleryRow({
  events,
  accountId,
  members,
  readReceipts = [],
  highlighted = null,
  renderEvent,
}: {
  events: readonly TimelineEvent[]
  accountId: string
  members: MembersStore
  /**
   * Receipts for the run's *last* event — the furthest point in the room a
   * reader has reached, which is what "seen by" means for a group of images.
   * Deliberately not per cell: receipts change on every ephemeral update, and
   * a per-cell rendering would redraw the grid constantly.
   */
  readReceipts?: readonly ReadReceipt[]
  /** The jump target, when a deep link lands inside this run. */
  highlighted?: string | null
  /**
   * Render one event as an ordinary row. Used by the expander, which is what
   * keeps *every* per-event affordance — reply, edit, delete, react, thread,
   * inspect, retry — reachable without the gallery reimplementing any of it.
   */
  renderEvent: (event: TimelineEvent) => preact.ComponentChildren
}) {
  // Session-scoped, so leaving the room and coming back keeps the choice.
  const eventIds = useMemo(() => events.map((e) => e.event_id), [events])
  const [expanded, setExpanded] = useGalleryExpansion(eventIds)
  const viewer = useMediaViewer()
  const gridRef = useRef<HTMLUListElement>(null)
  /**
   * Roving tabindex, keyed by **event id** rather than by position.
   *
   * A positional index silently assumed every event renders a focusable cell.
   * It does not: a still-uploading echo renders a bare `<img>` (there is
   * nothing on the server to open yet), a cell whose bytes have not arrived is
   * a skeleton, and an event whose media fails to resolve renders nothing at
   * all. The index then addressed a different cell than it named — in a
   * ready/pending/ready run, ArrowRight from the first cell focused the third
   * — and a run whose first event was pending had no tab stop anywhere,
   * because the cell holding `tabIndex={0}` was not focusable.
   */
  const [focusedId, setFocusedId] = useState<string | null>(null)

  const sender = events[0].sender
  const senderDisplay = members.displayName(sender)

  /**
   * Which cell owns the grid's single tab stop before anything is focused.
   *
   * A still-uploading echo can never hold it — it renders no control — so the
   * stop goes to the first event that will. Decided from the data rather than
   * the DOM because it has to be right on the very first render, before any
   * cell has resolved its bytes.
   */
  const firstNavigableId =
    events.find((event) => eventMedia(event)?.pending === false)?.event_id ??
    events[0].event_id
  const tabStopId = focusedId ?? firstNavigableId

  /**
   * Which cells may load themselves, walked in order until the run's
   * cumulative bytes pass the budget.
   *
   * Decided here because this is the only place the run's total is known — a
   * cell can see its own size and nothing else, which is what made the old
   * per-image rule defer everything once real photos arrived.
   *
   * An unknown size counts against the budget at its full weight rather than
   * zero: unknown is exactly the case that might be enormous.
   */
  const withinBudget = useMemo(() => {
    let used = 0
    return events.map((event) => {
      const media = eventMedia(event)?.media
      // Nothing to spend: the server or the sender already has a thumbnail.
      if (media == null || !media.encrypted || media.thumbnailUrl !== null) {
        return true
      }
      if (used >= GALLERY_EAGER_BYTES) {
        return false
      }
      used += media.size ?? GALLERY_EAGER_BYTES
      return true
    })
  }, [events])

  /** Only the images that actually carry a caption; usually none or one. */
  const captioned = events
    .map((event, index) => ({
      event,
      caption: eventMedia(event)?.media.caption ?? null,
      position: index + 1,
    }))
    .filter(
      (
        entry,
      ): entry is { event: TimelineEvent; caption: string; position: number } =>
        entry.caption !== null && entry.caption !== '',
    )

  if (expanded) {
    return (
      <>
        {events.map((event) => (
          // Keyed on the fragment itself, per WCR-01.
          <Fragment key={event.event_id}>{renderEvent(event)}</Fragment>
        ))}
        <li class="event-row gallery-expander-row">
          <button
            type="button"
            class="ghost gallery-expander"
            aria-expanded={true}
            onClick={() => setExpanded(false)}
          >
            Image gallery
          </button>
        </li>
      </>
    )
  }

  /**
   * The cells that can actually take focus, in document order, paired with the
   * event each belongs to. Read from the DOM because focusability is decided
   * inside the cell — whether its bytes arrived, whether the byte budget
   * deferred it — and is not knowable from `events` alone.
   */
  const navigableCells = (): { el: HTMLElement; eventId: string }[] => {
    const grid = gridRef.current
    if (grid === null) {
      return []
    }
    return [...grid.querySelectorAll<HTMLElement>(NAVIGABLE)].flatMap((el) => {
      const eventId = el.closest('.gallery-cell')?.getAttribute('data-event-id')
      return eventId === null || eventId === undefined ? [] : [{ el, eventId }]
    })
  }

  /**
   * Move by `delta` *within the focusable cells*, not within `events`.
   *
   * The consequence worth knowing: with a pending cell in the middle of a row,
   * Up/Down no longer moves exactly `GALLERY_COLUMNS` visual cells. Landing
   * one cell off is a far smaller cost than landing on a cell that cannot take
   * focus, which strands the reader on `<body>` with no way back into the grid.
   */
  const moveFocus = (fromId: string, delta: number) => {
    const cells = navigableCells()
    const from = cells.findIndex((cell) => cell.eventId === fromId)
    if (from === -1) {
      return false
    }
    const next = from + delta
    if (next < 0 || next >= cells.length) {
      return false
    }
    setFocusedId(cells[next].eventId)
    cells[next].el.focus()
    return true
  }

  // 2-D grid navigation, the same idiom `MessageEventRow` uses for the
  // reaction picker — with a fixed column count, so no `getComputedStyle`.
  const onCellKeyDown = (eventId: string) => (event: KeyboardEvent) => {
    const delta =
      event.key === 'ArrowLeft'
        ? -1
        : event.key === 'ArrowRight'
          ? 1
          : event.key === 'ArrowUp'
            ? -GALLERY_COLUMNS
            : event.key === 'ArrowDown'
              ? GALLERY_COLUMNS
              : 0
    if (delta === 0 || event.altKey || event.ctrlKey || event.metaKey) {
      return
    }
    if (moveFocus(eventId, delta)) {
      event.preventDefault()
      event.stopPropagation()
    }
  }

  return (
    <li
      class="event-row gallery-row"
      data-event-id={events[0].event_id}
      data-gallery="true"
    >
      <UserAvatar
        accountId={accountId}
        userId={sender}
        displayName={senderDisplay}
        member={members.members.value.get(sender)}
      />
      <div class="event-content">
        <div class="event-head">
          <span class="event-sender">{senderDisplay}</span>
          <button
            type="button"
            class="ghost gallery-expander"
            aria-expanded={false}
            onClick={() => setExpanded(true)}
          >
            Ungroup images
          </button>
        </div>
        <ul
          ref={gridRef}
          class="gallery-grid"
          // The one place the column count is written down. `ArrowUp` and
          // `ArrowDown` move by `GALLERY_COLUMNS`, so a CSS literal that
          // drifted from it would move focus to a cell the reader is not
          // looking at.
          style={{ '--gallery-columns': GALLERY_COLUMNS }}
          aria-label={`${events.length} images from ${senderDisplay}`}
        >
          {events.map((event, index) => {
            const resolved = eventMedia(event)
            if (resolved === null) {
              return null
            }
            return (
              <MediaGalleryCell
                key={event.event_id}
                accountId={accountId}
                eventId={event.event_id}
                highlighted={event.event_id === highlighted}
                resolved={resolved}
                position={index + 1}
                total={events.length}
                focusable={event.event_id === tabStopId}
                withinBudget={withinBudget[index]}
                onOpen={() => {
                  setFocusedId(event.event_id)
                  viewer?.open(event.event_id)
                }}
                onCellKeyDown={onCellKeyDown(event.event_id)}
              />
            )
          })}
        </ul>
        {captioned.length > 0 && (
          /*
           * Captions live beneath the grid rather than on the cells: a square
           * thumbnail has no room for text, and overlaying it hides the very
           * thing the reader opened the gallery to see. Numbered when there is
           * more than one, so a caption can be tied to its image.
           *
           * This is also what makes an edited caption visible without the run
           * having to break — see `groupable` in `group-media-runs`.
           */
          <ul class="gallery-captions">
            {captioned.map(({ event, caption, position }) => (
              <li key={event.event_id} class="gallery-caption">
                {captioned.length > 1 && (
                  <span class="gallery-caption-index" aria-hidden="true">
                    {position}
                  </span>
                )}
                <span>
                  {captioned.length > 1 ? `Image ${position}: ` : ''}
                  {caption}
                </span>
              </li>
            ))}
          </ul>
        )}
        {readReceipts.length > 0 && (
          <ReadReceiptsSummary
            receipts={readReceipts}
            members={members}
            accountId={accountId}
          />
        )}
      </div>
    </li>
  )
}
