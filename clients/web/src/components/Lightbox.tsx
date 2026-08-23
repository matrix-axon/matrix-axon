import type { ComponentChildren } from 'preact'
import { useEffect, useRef, useState } from 'preact/hooks'
import { useMediaBlob } from '../media/use-media-blob'
import { useShortcuts } from '../shortcuts'
import { BodyPortal } from './BodyPortal'
import { useSwipePaging } from '../media/use-swipe-paging'
import { useModalFocus } from './use-modal-focus'

/**
 * Elements a click must *not* dismiss on: the media being viewed, its caption,
 * and any control. Everything else in the overlay counts as backdrop.
 */
const DISMISS_EXEMPT = 'video, audio, img, iframe, figcaption, button, a'

/**
 * How long after toggling immersive mode a further tap on the image is
 * ignored.
 *
 * A double-click, or the impatient second tap people give a control that did
 * not seem to respond, otherwise toggles twice — the chrome flashes back and
 * disappears again, which reads as "I cannot get the buttons to return".
 * Nobody needs to toggle twice inside a third of a second, so swallowing the
 * second event costs nothing. Time-based rather than `event.detail > 1`,
 * because touch does not set a click count the way a mouse does.
 */
const TOGGLE_GRACE_MS = 300

/**
 * Paging across a sequence of images (ADR 0081). Optional: without it the
 * lightbox is exactly the single-media view it has always been, which is what
 * `MediaPreview` and search results still use.
 */
export interface LightboxPaging {
  /** Zero-based position of the open image within the loaded sequence. */
  index: number
  total: number
  /** Position within the gallery run this image belongs to, if any. */
  run?: { index: number; total: number } | null
  hasPrev: boolean
  hasNext: boolean
  /** Auto-paginating into history at the oldest end. */
  loadingOlder: boolean
  /** No more history to load — the oldest end is genuinely reached. */
  atOldest: boolean
  onPrev: () => void
  onNext: () => void
}

/**
 * A full-viewport view of one piece of media (ADR 0064; generalised beyond
 * images in ADR 0072). Reuses the app's `.overlay` modal shell and follows the
 * shared modal contract (`useModalFocus` + capture-phase Escape), so there are
 * always three ways back to the timeline: Escape, the ✕, and a tap anywhere
 * that is not the media itself.
 *
 * The shell owns presentation only — loading is the caller's, because an image,
 * a video and a PDF want different elements and different failure text.
 *
 * Given `paging` it becomes a viewer over a sequence (ADR 0081), gaining
 * prev/next controls, arrow keys, swipe, and an announced position.
 */
export function Lightbox({
  label,
  caption,
  onClose,
  paging,
  onSave,
  saving,
  saveError,
  actionError,
  actions,
  restoreTo,
  children,
}: {
  /** Accessible name for the dialog — the caption or filename. */
  label: string
  caption: ComponentChildren
  onClose: () => void
  paging?: LightboxPaging
  /**
   * Save the displayed object to the device. Offered only once the caller has
   * confirmed the bytes decoded — the proxy serves undecryptable ciphertext
   * with a 200, and saving that under a plausible `.jpg` name is worse than
   * offering nothing.
   */
  onSave?: () => void
  saving?: boolean
  /** Message from a save that failed, announced assertively. */
  saveError?: string | null
  /** Message from a contextual event action that failed. */
  actionError?: string | null
  /** Contextual controls for the event currently shown in the viewer. */
  actions?: ComponentChildren
  /** See `useModalFocus`; the viewer points this at the current image's row. */
  restoreTo?: () => HTMLElement | null
  children: ComponentChildren
}) {
  const closeRef = useRef<HTMLButtonElement>(null)
  const { containerRef } = useModalFocus<HTMLDivElement>({
    initialFocus: () => closeRef.current,
    restoreTo,
  })
  const swipe = useSwipePaging({
    onOlder: () => paging?.onPrev(),
    onNewer: () => paging?.onNext(),
    onDismiss: onClose,
  })
  /**
   * Immersive mode: a tap on the image itself hides the overlay chrome so the
   * photo can be seen unobstructed, and another tap brings it back.
   *
   * The chrome is hidden with `opacity` and `pointer-events`, deliberately not
   * `display: none` or the `hidden` attribute. `collectFocusable` skips hidden
   * elements, so removing them would empty the focus trap — and the trap
   * early-returns on an empty list, which would let Tab escape the dialog
   * entirely. Staying focusable also gives the mode a free way back for
   * keyboard users: `focusin` restores the chrome, so tabbing to a control
   * reveals it the moment it takes focus.
   */
  const [chromeHidden, setChromeHidden] = useState(false)
  const lastToggleAt = useRef(0)

  // Bound imperatively rather than as an `onFocusIn` prop: `focusin` is one of
  // the few events whose JSX prop name does not resolve uniformly, and this
  // listener is the only thing standing between a keyboard user and a set of
  // invisible controls — too load-bearing to leave to prop-name resolution.
  useEffect(() => {
    const container = containerRef.current
    if (container === null) {
      return
    }
    const reveal = (event: FocusEvent) => {
      const target = event.target
      // Only a *control* taking focus should reveal the chrome. The image
      // wrapper is itself focusable (`tabindex="0"`), so clicking it focuses
      // it — and without this guard the reveal would fire on every tap and
      // immediately undo the toggle that the same click is about to perform,
      // leaving immersive mode impossible to exit. Caught in a real browser
      // only: jsdom's synthetic click does not move focus.
      if (
        target instanceof HTMLElement &&
        target !== container &&
        target.closest('.lightbox-image') === null
      ) {
        setChromeHidden(false)
      }
    }
    container.addEventListener('focusin', reveal)
    return () => container.removeEventListener('focusin', reveal)
  }, [containerRef])

  // Topmost surface: claim Escape first via capture, like the other modals.
  // Arrows page when there is a sequence; there are no competing
  // document-level arrow bindings (the reaction picker's are element-scoped).
  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        onClose()
      },
      ArrowLeft: (event) => {
        if (paging === undefined) {
          return
        }
        event.preventDefault()
        paging.onPrev()
      },
      ArrowRight: (event) => {
        if (paging === undefined) {
          return
        }
        event.preventDefault()
        paging.onNext()
      },
    },
    { whileTyping: true, capture: true },
  )

  return (
    <BodyPortal>
      <div
        ref={containerRef}
        tabIndex={-1}
        class={`overlay lightbox${chromeHidden ? ' lightbox-immersive' : ''}`}
        {...swipe}
        role="dialog"
        aria-modal="true"
        aria-label={label}
        onClick={(event) => {
          // Anything that is not the media itself is backdrop. Testing
          // `target === currentTarget` instead would make only the literal
          // overlay dismiss, and a letterboxed video leaves most of the screen
          // covered by the figure and its wrapper — tapping the dark area
          // beside or below the player would then do nothing, which reads as
          // broken. `closest` also keeps a click on a `<video>`'s own controls
          // from closing the thing it is scrubbing.
          if (!(event.target instanceof Element)) {
            onClose()
            return
          }
          // A tap on the image toggles immersive mode. Scoped to
          // `.lightbox-image` rather than any `img`, so a click on a video's
          // own controls or a PDF page (ADR 0072) is untouched.
          if (event.target.closest('.lightbox-image') !== null) {
            // A swipe that moved still synthesises a click; that is paging,
            // not a tap, and must not also toggle the chrome.
            if (
              !swipe.swipedRecently() &&
              Date.now() - lastToggleAt.current >= TOGGLE_GRACE_MS
            ) {
              lastToggleAt.current = Date.now()
              const next = !chromeHidden
              setChromeHidden(next)
              // Entering immersive mode, park focus on the dialog itself. The
              // mount focus sits on the ✕, and leaving it there would mean an
              // *invisible* button still holds focus — Enter or Space would
              // then close the viewer with nothing on screen to explain why.
              // The container is `tabindex="-1"`, so it holds focus without
              // becoming a tab stop, and the first Tab lands on a real control
              // and reveals the chrome again.
              //
              // Done here rather than inside the state updater: an updater may
              // be invoked more than once, which toggled the state twice and
              // made immersive mode impossible to exit.
              if (next) {
                containerRef.current?.focus()
              }
            }
            return
          }
          if (event.target.closest(DISMISS_EXEMPT) === null) {
            onClose()
          }
        }}
      >
        <div class="lightbox-toolbar">
          {actions}
          {onSave !== undefined && (
            <button
              type="button"
              class="ghost lightbox-save"
              aria-label="Save image to device"
              disabled={saving === true}
              onClick={onSave}
            >
              {saving === true ? '…' : '⤓'}
            </button>
          )}
          <button
            ref={closeRef}
            type="button"
            class="ghost lightbox-close"
            aria-label="Close"
            onClick={onClose}
          >
            ✕
          </button>
        </div>
        {saveError !== null && saveError !== undefined && (
          // `alert`, not the polite paging status: this is the outcome of
          // something the reader just did, and it must not wait its turn
          // behind a position announcement.
          <p class="lightbox-save-error" role="alert">
            {saveError}
          </p>
        )}
        {actionError !== null && actionError !== undefined && (
          <p class="lightbox-action-error" role="alert">
            {actionError}
          </p>
        )}
        {paging !== undefined && (
          <button
            type="button"
            class="ghost lightbox-page lightbox-prev"
            aria-label="Previous image"
            // The attribute, not just `aria-disabled`: `collectFocusable`
            // filters `button:not([disabled])`, so an aria-only version would
            // leave a dead stop inside the focus trap.
            disabled={!paging.hasPrev}
            onClick={paging.onPrev}
          >
            ‹
          </button>
        )}
        <figure class="lightbox-figure">
          {children}
          {caption ? (
            <figcaption class="lightbox-caption">{caption}</figcaption>
          ) : null}
        </figure>
        {paging !== undefined && (
          <>
            <button
              type="button"
              class="ghost lightbox-page lightbox-next"
              aria-label="Next image"
              disabled={!paging.hasNext}
              onClick={paging.onNext}
            >
              ›
            </button>
            {/*
              Focus stays on whatever control the user is operating when the
              image changes, so the change has to be announced. A changed
              `aria-label` on an already-mounted dialog is not reliably
              announced; a live region is.
            */}
            {/*
              Always in the DOM, even when it has nothing to say: a live
              region added at the moment its text changes is unreliably
              announced. `:empty` collapses it to nothing visually instead.

              Only the position *within a gallery* is shown. The image's
              index among every image in the loaded timeline is a number the
              reader has no use for — and actively misleads, because
              back-paginating changes it without anything being sent.
            */}
            <p class="lightbox-status" role="status" aria-live="polite">
              {paging.loadingOlder
                ? 'Loading older messages'
                : paging.run != null
                  ? `${paging.run.index + 1}/${paging.run.total}`
                  : ''}
              {!paging.loadingOlder && !paging.hasPrev && paging.atOldest
                ? `${paging.run != null ? ' — ' : ''}oldest image`
                : ''}
            </p>
          </>
        )}
      </div>
    </BodyPortal>
  )
}

/**
 * The lightbox's image body: the full-size object URL, loaded eagerly and never
 * a thumbnail.
 */
export function LightboxImage({
  accountId,
  mxcUrl,
  alt,
  onDecoded,
}: {
  accountId: string
  mxcUrl: string
  alt: string
  /**
   * Whether the bytes actually decoded as an image. The media proxy returns
   * **200 with raw ciphertext** when it lacks the decryption key, so a ready
   * blob is not necessarily a picture — it surfaces only here, at decode.
   * `MediaImage` has always caught this; this one did not, and rendered a
   * broken-image icon with no explanation.
   */
  onDecoded?: (decoded: boolean) => void
}) {
  const { state } = useMediaBlob(accountId, mxcUrl, { eager: true })
  const [decodeFailed, setDecodeFailed] = useState(false)
  const onDecodedRef = useRef(onDecoded)
  useEffect(() => {
    onDecodedRef.current = onDecoded
  })

  // A new object means a fresh verdict — paging must not carry the previous
  // image's failure, or its success, onto the next one.
  useEffect(() => {
    setDecodeFailed(false)
    onDecodedRef.current?.(false)
  }, [mxcUrl])

  return (
    <div tabindex={0} class="lightbox-image">
      {decodeFailed ? (
        <p class="muted placeholder">
          Encrypted media — server could not decrypt
        </p>
      ) : state.status === 'ready' && state.url !== undefined ? (
        <img
          src={state.url}
          alt={alt}
          onLoad={() => onDecodedRef.current?.(true)}
          onError={() => {
            setDecodeFailed(true)
            onDecodedRef.current?.(false)
          }}
        />
      ) : state.status === 'error' ? (
        <p class="muted placeholder">Could not load image</p>
      ) : (
        <div class="media-skeleton" aria-hidden="true" />
      )}
    </div>
  )
}
