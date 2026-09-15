import { useEffect, useState } from 'preact/hooks'
import type { EventMedia } from '../media/event-media'
import { useMediaBlob } from '../media/use-media-blob'
import { useThumbnailFallback } from '../media/use-thumbnail-fallback'
import type {
  MessageGestureAction,
  MessageGesturePreferences,
} from '../stores/message-gestures'
import { EventActionIcon } from './EventActionIcon'
import { messageGestureActionLabel } from './message-gesture-actions'
import { useTouchMessageGestures } from './use-touch-message-gestures'

/** The square a gallery cell requests, in CSS px. */
const CELL_SIZE = 320

/**
 * How many bytes one gallery may pull automatically before the rest of its
 * cells wait for a tap.
 *
 * The first rule here was a *per-image* ceiling of 256 KB, calibrated from an
 * early probe whose 13 images had a 65 KB median — mostly screenshots. Against
 * real camera photos, which run 1–3 MB each, that deferred every cell and
 * turned a gallery into a wall of grey tiles: the gate meant to catch the
 * exceptional case was catching the normal one.
 *
 * A run budget is the honest shape of the concern. Cells already load lazily
 * through an IntersectionObserver and `media-service` caps concurrency, so the
 * risk was never unbounded parallelism — it is the total pulled to draw one
 * row. Cells load in order until the running total passes this, which makes
 * the decision deterministic by position: the first cells always fill in, and
 * a four-photo post lands well inside it.
 */
export const GALLERY_EAGER_BYTES = 8 * 1024 * 1024

function humanSize(bytes: number): string {
  return bytes >= 1024 * 1024
    ? `${(bytes / (1024 * 1024)).toFixed(1)} MB`
    : `${Math.max(1, Math.round(bytes / 1024))} KB`
}

/**
 * One image in a gallery grid (ADR 0081).
 *
 * Requests a **cropped** square rather than a scaled thumbnail: the grid is
 * square by construction, and `keyOf` in `media-service` includes the method,
 * so a cell's request gets its own cache slot and never collides with the
 * scaled one `MediaImage` asks for.
 */
export function MediaGalleryCell({
  accountId,
  eventId,
  highlighted = false,
  resolved,
  position,
  total,
  focusable,
  withinBudget,
  onOpen,
  onCellKeyDown,
  gestures = null,
}: {
  accountId: string
  /** The image's own event, so a jump link can find and highlight this cell. */
  eventId: string
  highlighted?: boolean
  resolved: EventMedia
  /** 1-based, for the accessible name — "Image 3 of 7" beats "button". */
  position: number
  total: number
  /** Roving tabindex: exactly one cell in a grid is a tab stop. */
  focusable: boolean
  /**
   * Whether the run's byte budget still had room for this cell. Decided by the
   * row, which is the only place the run's cumulative size is known.
   */
  withinBudget: boolean
  onOpen: () => void
  onCellKeyDown: (event: KeyboardEvent) => void
  gestures?: {
    preferences: MessageGesturePreferences
    onAction: (action: MessageGestureAction) => void
    feedback: string | null
    reactionBurst: string | null
    confirmingDelete: boolean
    onConfirmDelete: () => void
    onCancelDelete: () => void
  } | null
}) {
  const { media, previewUrl, pending } = resolved
  const [loadRequested, setLoadRequested] = useState(false)

  // Only an image with nothing but its full-size object to show can be
  // deferred; anything the server or the sender thumbnailed is cheap already.
  const mustFetchFullSize = media.encrypted && media.thumbnailUrl === null
  const deferred = mustFetchFullSize && !withinBudget && !loadRequested

  // The load outcome is fed back into the hook so a thumbnail that fails —
  // a homeserver that cannot crop, a purged sender thumbnail — falls back to
  // the full-size object instead of leaving an empty cell. Passing a constant
  // here made the shared recovery inert, which is the whole reason the hook
  // was extracted from `MediaImage`.
  const [loadStatus, setLoadStatus] = useState<'idle' | 'error'>('idle')
  const { displayUrl, thumbnail } = useThumbnailFallback(media, loadStatus, {
    size: CELL_SIZE,
    method: 'crop',
  })
  const { ref, state } = useMediaBlob<HTMLDivElement>(
    accountId,
    previewUrl !== null || deferred ? null : displayUrl,
    { thumbnail },
  )
  useEffect(() => {
    setLoadStatus(state.status === 'error' ? 'error' : 'idle')
  }, [state.status])

  const alt = media.caption ?? media.filename
  const label = `Image ${position} of ${total}, ${media.filename}`
  const gestureEligible =
    gestures !== null &&
    previewUrl === null &&
    !pending &&
    !deferred &&
    state.status === 'ready'
  const touchGestures = useTouchMessageGestures<HTMLLIElement>({
    eligible: gestureEligible,
    preferences: gestures?.preferences ?? null,
    openControlSelector: '.gallery-cell-open',
    onAction: (action) => gestures?.onAction(action),
    onSingleTap: onOpen,
  })
  const cellClass = `gallery-cell${highlighted ? ' highlighted' : ''}${touchGestures.touchHoldEnabled ? ' touch-hold-enabled' : ''}${touchGestures.swipeReveal !== null ? ' gesture-swipe-reveal' : ''}${touchGestures.swipeArmed ? ' gesture-swipe-armed' : ''}${touchGestures.swipeSettling ? ' gesture-swipe-settling' : ''}`
  const gestureAttributes = gestureEligible
    ? {
        ref: touchGestures.surfaceRef,
        onPointerDown: touchGestures.onPointerDown,
        onPointerMove: touchGestures.onPointerMove,
        onPointerCancel: touchGestures.onPointerCancel,
        onPointerUp: touchGestures.onPointerUp,
        onContextMenu: touchGestures.onContextMenu,
        onClickCapture: touchGestures.onClickCapture,
      }
    : {}

  if (deferred) {
    const size = media.size
    return (
      <li class={cellClass} data-event-id={eventId}>
        <button
          type="button"
          class="gallery-cell-deferred"
          tabIndex={focusable ? 0 : -1}
          aria-label={`Load image ${position} of ${total}, ${media.filename}${
            size === undefined ? '' : `, ${humanSize(size)}`
          }`}
          onClick={() => setLoadRequested(true)}
          onKeyDown={onCellKeyDown}
        >
          <span class="gallery-cell-name">{media.filename}</span>
          {size !== undefined && (
            <span class="gallery-cell-size">{humanSize(size)}</span>
          )}
        </button>
      </li>
    )
  }

  return (
    <li class={cellClass} data-event-id={eventId} {...gestureAttributes}>
      <div ref={ref} class="gallery-cell-box">
        {previewUrl !== null ? (
          // Still uploading: the local file, and not a button — there is
          // nothing on the server to open yet.
          <img
            class="gallery-cell-image"
            src={previewUrl}
            alt={alt}
            decoding="async"
          />
        ) : state.status === 'ready' && state.url !== undefined ? (
          <button
            type="button"
            class="gallery-cell-open"
            tabIndex={focusable ? 0 : -1}
            aria-label={label}
            onClick={onOpen}
            onKeyDown={onCellKeyDown}
          >
            <img
              class="gallery-cell-image"
              src={state.url}
              alt={alt}
              decoding="async"
            />
          </button>
        ) : state.status === 'error' ? (
          // Focusable, and part of the roving set: a cell whose bytes failed
          // is still one of the sender's images. Skipping it would make the
          // grid's announced count disagree with what arrow keys can reach,
          // and would silently hide the failure from a keyboard reader.
          <p
            class="muted placeholder gallery-cell-failed"
            tabIndex={focusable ? 0 : -1}
            aria-label={`${label} — could not load`}
            onKeyDown={onCellKeyDown}
          >
            Could not load
          </p>
        ) : (
          <div class="media-skeleton" aria-hidden="true" />
        )}
      </div>
      {pending && <span class="gallery-cell-pending" aria-hidden="true" />}
      {touchGestures.swipeReveal !== null && (
        <span class="gesture-swipe-affordance" aria-hidden="true">
          <EventActionIcon name={touchGestures.swipeReveal} />
          {messageGestureActionLabel(touchGestures.swipeReveal)}
        </span>
      )}
      {gestures?.reactionBurst !== null &&
        gestures?.reactionBurst !== undefined && (
          <span class="message-reaction-burst" aria-hidden="true">
            {gestures.reactionBurst}
          </span>
        )}
      {gestures?.feedback !== null && gestures?.feedback !== undefined && (
        <span
          class="message-gesture-feedback gallery-gesture-feedback"
          role="status"
          aria-label={`Image ${position} of ${total}: ${gestures.feedback}`}
        >
          {gestures.feedback}
        </span>
      )}
      {gestures?.confirmingDelete && (
        <span
          class="gallery-gesture-confirm"
          role="group"
          aria-label={`Delete image ${position} of ${total}`}
        >
          <button
            type="button"
            class="danger"
            onClick={gestures.onConfirmDelete}
          >
            Delete
          </button>
          <button type="button" onClick={gestures.onCancelDelete}>
            Cancel
          </button>
        </span>
      )}
    </li>
  )
}
