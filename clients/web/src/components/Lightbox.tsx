import { createContext, type ComponentChildren } from 'preact'
import {
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from 'preact/hooks'
import {
  FIT,
  MIN_SCALE,
  panBy,
  ZOOM_STEP,
  zoomAt,
  zoomTransform,
  type Point,
  type ZoomState,
} from '../media/image-zoom'
import { useMediaBlob } from '../media/use-media-blob'
import type { SniffedFormat } from '../media/media-service'
import type { ParsedMedia } from '../media/parse-media'
import { imageDecodeFailureMessage } from '../media/image-format'
import { useShortcuts } from '../shortcuts'
import { BodyPortal } from './BodyPortal'
import { useSwipePaging } from '../media/use-swipe-paging'
import { useModalFocus } from './use-modal-focus'

/**
 * Elements a click must *not* dismiss on: the media being viewed, its caption,
 * and any control. Everything else in the overlay counts as backdrop.
 *
 * `label` and `input` are here because a file picker in the `actions` slot is
 * a label wrapping a hidden input — the only way to open a picker from the
 * user's own click, since a synthetic `.click()` loses the gesture. Without
 * them the lightbox would dismiss itself out from under the file dialog.
 */
const DISMISS_EXEMPT =
  'video, audio, img, iframe, figcaption, button, a, label, input'

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
 * How long after a pan or pinch a click on the image is still taken as part
 * of it. A drag still ends in a synthesised click, which would otherwise also
 * toggle immersive mode.
 */
const ZOOM_CLICK_GRACE_MS = 400

/** Travel, in CSS pixels, before a press on a zoomed image counts as a drag. */
const ZOOM_DRAG_SLOP_PX = 4

/**
 * The viewer's image zoom (`media/image-zoom.ts`), shared with
 * `LightboxImage`. The overlay owns the gestures, because a pinch can start
 * anywhere on the image and the chrome around it has to know; the image only
 * applies the transform, and registers itself, which is what offers zoom at
 * all. A video or a PDF never registers, so it gets no zoom controls.
 */
interface LightboxZoom {
  transform: string | undefined
  register: (image: HTMLImageElement | null) => void
  reset: () => void
}

const LightboxZoomContext = createContext<LightboxZoom | null>(null)

/** A WebKit `GestureEvent`: a trackpad pinch in Safari and the macOS shell. */
type GestureEvent = UIEvent & {
  scale: number
  clientX: number
  clientY: number
}

/** `Ctrl`/`⌘` zoom keys, every spelling `chordOf` can produce for them. */
const ZOOM_IN_CHORDS = ['mod+=', 'mod++', 'mod+shift+=', 'mod+shift++']

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
   * Save the displayed object to the device. Withheld only while the bytes are
   * still in flight. Once they have arrived it is offered whatever they turned
   * out to be — an image that would not decode is exactly the case where
   * opening it in another application is the one thing left that helps, and
   * ADR 0101's narrower gate (identifiable formats only) is withdrawn (#359).
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
  const [zoom, setZoom] = useState<ZoomState>(FIT)
  const zoomRef = useRef(zoom)
  const imageRef = useRef<HTMLImageElement | null>(null)
  const [hasImage, setHasImage] = useState(false)
  const pointers = useRef(new Map<number, Point>())
  const gesture = useRef<
    | { kind: 'pan'; start: Point; from: ZoomState }
    | { kind: 'pinch'; distance: number; middle: Point; from: ZoomState }
    | null
  >(null)
  const gestureStart = useRef(0)
  const zoomedAt = useRef(0)
  const trackpadPinch = useRef<ZoomState | null>(null)
  /**
   * The zoom the image is actually drawn with. `zoomRef` is the zoom asked
   * for, which runs ahead of the DOM until the next render; a box measured
   * with `getBoundingClientRect` carries this one. Set after each commit, when
   * `LightboxImage` has applied its transform.
   */
  const renderedZoom = useRef<ZoomState>(FIT)
  useLayoutEffect(() => {
    renderedZoom.current = zoom
  }, [zoom])

  const applyZoom = (next: ZoomState) => {
    zoomRef.current = next
    setZoom(next)
  }
  const resetZoom = useCallback(() => {
    zoomRef.current = FIT
    setZoom(FIT)
  }, [])
  const register = useCallback((image: HTMLImageElement | null) => {
    imageRef.current = image
    setHasImage(image !== null)
    if (image === null) {
      zoomRef.current = FIT
      setZoom(FIT)
    }
  }, [])

  /**
   * The image's untransformed size, and its untransformed centre on screen.
   * The transform scales about the centre, so the rendered centre has only
   * moved by the translation — the *rendered* one. Subtracting the zoom asked
   * for instead put the centre off by however far that had run ahead of the
   * DOM, and a pinch then drifted away from the pointer.
   */
  const imageGeometry = () => {
    const image = imageRef.current
    if (image === null) {
      return null
    }
    const rect = image.getBoundingClientRect()
    const { x, y } = renderedZoom.current
    return {
      size: { width: image.offsetWidth, height: image.offsetHeight },
      centre: {
        x: rect.left + rect.width / 2 - x,
        y: rect.top + rect.height / 2 - y,
      },
    }
  }

  /**
   * Zoom to `scale` about a screen point, or about what is in view now.
   * `from` is the state the scale is relative to: the current zoom, or the
   * zoom a trackpad pinch started at.
   */
  const zoomTo = (
    scale: number,
    at?: Point,
    from: ZoomState = zoomRef.current,
  ) => {
    const geometry = imageGeometry()
    if (geometry === null) {
      return
    }
    const anchor =
      at === undefined
        ? { x: from.x, y: from.y }
        : { x: at.x - geometry.centre.x, y: at.y - geometry.centre.y }
    applyZoom(zoomAt(from, scale, anchor, geometry.size))
  }

  /** Start a pan or pinch from whatever pointers are down now. */
  const beginGesture = () => {
    const down = [...pointers.current.values()]
    if (down.length >= 2) {
      const [a, b] = down
      gesture.current = {
        kind: 'pinch',
        distance: Math.hypot(a.x - b.x, a.y - b.y) || 1,
        middle: { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 },
        from: zoomRef.current,
      }
    } else if (down.length === 1 && zoomRef.current.scale > MIN_SCALE) {
      gesture.current = { kind: 'pan', start: down[0], from: zoomRef.current }
    } else {
      gesture.current = null
    }
  }

  const swipe = useSwipePaging({
    onOlder: () => paging?.onPrev(),
    onNewer: () => paging?.onNext(),
    onDismiss: onClose,
    // A zoomed image pans with one finger, and a pinch is two.
    blocked: () => zoomRef.current.scale > MIN_SCALE,
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

  // Ctrl/⌘-scroll zooms at the pointer, which is also how Chromium and Firefox
  // report a trackpad pinch; a plain scroll pans a zoomed image. Bound
  // imperatively because a wheel listener has to be non-passive to cancel the
  // page zoom, and Preact's `onWheel` makes no such promise.
  //
  // WebKit reports a trackpad pinch as `gesture*` events instead (Safari, and
  // the macOS shell's WKWebView), so those are taken too. iOS sends them for a
  // touch pinch as well; that one is the pointer handlers' to apply, so here it
  // is only cancelled, to keep the page itself from zooming.
  useEffect(() => {
    const container = containerRef.current
    if (container === null) {
      return
    }
    const onWheel = (event: WheelEvent) => {
      if (imageRef.current === null) {
        return
      }
      const unit =
        event.deltaMode === 1
          ? 16
          : event.deltaMode === 2
            ? window.innerHeight
            : 1
      if (event.ctrlKey || event.metaKey) {
        event.preventDefault()
        zoomedAt.current = Date.now()
        zoomTo(zoomRef.current.scale * Math.exp(-event.deltaY * unit * 0.01), {
          x: event.clientX,
          y: event.clientY,
        })
        return
      }
      const geometry = imageGeometry()
      if (zoomRef.current.scale > MIN_SCALE && geometry !== null) {
        event.preventDefault()
        applyZoom(
          panBy(
            zoomRef.current,
            { x: -event.deltaX * unit, y: -event.deltaY * unit },
            geometry.size,
          ),
        )
      }
    }
    const onGestureStart = (event: Event) => {
      if (imageRef.current === null) {
        return
      }
      event.preventDefault()
      trackpadPinch.current =
        pointers.current.size === 0 ? zoomRef.current : null
    }
    const onGestureChange = (event: Event) => {
      if (imageRef.current === null) {
        return
      }
      event.preventDefault()
      const from = trackpadPinch.current
      if (from === null) {
        return
      }
      const pinch = event as GestureEvent
      zoomedAt.current = Date.now()
      // `scale` is cumulative since `gesturestart`, so zoom from there.
      zoomTo(
        from.scale * pinch.scale,
        { x: pinch.clientX, y: pinch.clientY },
        from,
      )
    }
    const onGestureEnd = (event: Event) => {
      if (imageRef.current !== null) {
        event.preventDefault()
      }
      trackpadPinch.current = null
    }
    container.addEventListener('wheel', onWheel, { passive: false })
    container.addEventListener('gesturestart', onGestureStart)
    container.addEventListener('gesturechange', onGestureChange)
    container.addEventListener('gestureend', onGestureEnd)
    return () => {
      container.removeEventListener('wheel', onWheel)
      container.removeEventListener('gesturestart', onGestureStart)
      container.removeEventListener('gesturechange', onGestureChange)
      container.removeEventListener('gestureend', onGestureEnd)
    }
    // The handlers read the zoom through refs, so one subscription serves.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [containerRef])

  // Topmost surface: claim Escape first via capture, like the other modals.
  // Arrows page when there is a sequence; there are no competing
  // document-level arrow bindings (the reaction picker's are element-scoped).
  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        // Staged, like the app's other Escapes: a zoomed image first goes
        // back to fitting the screen.
        if (zoomRef.current.scale > MIN_SCALE) {
          resetZoom()
          return
        }
        onClose()
      },
      // The image's zoom, not the page's: inside the viewer, the shell's own
      // Ctrl/⌘ zoom (ADR 0107) would only enlarge the chrome around a picture
      // that stays capped at the viewport.
      ...Object.fromEntries(
        ZOOM_IN_CHORDS.map((chord) => [
          chord,
          (event: KeyboardEvent) => {
            if (!hasImage) return
            event.preventDefault()
            zoomTo(zoomRef.current.scale * ZOOM_STEP)
          },
        ]),
      ),
      'mod+-': (event) => {
        if (!hasImage) return
        event.preventDefault()
        zoomTo(zoomRef.current.scale / ZOOM_STEP)
      },
      'mod+0': (event) => {
        if (!hasImage) return
        event.preventDefault()
        resetZoom()
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
  // The bare keys, as in an image viewer. Not while typing: a reaction search
  // in the toolbar must still be able to take a `-` or a `0`.
  useShortcuts(
    {
      '+': (event) => {
        if (!hasImage) return
        event.preventDefault()
        zoomTo(zoomRef.current.scale * ZOOM_STEP)
      },
      '=': (event) => {
        if (!hasImage) return
        event.preventDefault()
        zoomTo(zoomRef.current.scale * ZOOM_STEP)
      },
      '-': (event) => {
        if (!hasImage) return
        event.preventDefault()
        zoomTo(zoomRef.current.scale / ZOOM_STEP)
      },
      '0': (event) => {
        if (!hasImage) return
        event.preventDefault()
        resetZoom()
      },
    },
    { capture: true },
  )

  const zoomed = zoom.scale > MIN_SCALE
  const zoomContext: LightboxZoom = {
    transform: zoomTransform(zoom),
    register,
    reset: resetZoom,
  }

  return (
    <BodyPortal>
      <div
        ref={containerRef}
        tabIndex={-1}
        class={`overlay lightbox${chromeHidden ? ' lightbox-immersive' : ''}${zoomed ? ' lightbox-zoomed' : ''}`}
        {...swipe}
        onPointerDown={(event) => {
          if (
            !hasImage ||
            !(event.target instanceof Element) ||
            event.target.closest('.lightbox-image img') === null ||
            (event.pointerType === 'mouse' && event.button !== 0)
          ) {
            return
          }
          pointers.current.set(event.pointerId, {
            x: event.clientX,
            y: event.clientY,
          })
          try {
            event.target.setPointerCapture(event.pointerId)
          } catch {
            // A synthetic or already-released pointer; the drag still works
            // for as long as it stays over the image.
          }
          gestureStart.current = Date.now()
          beginGesture()
        }}
        onPointerMove={(event) => {
          if (!pointers.current.has(event.pointerId)) {
            return
          }
          pointers.current.set(event.pointerId, {
            x: event.clientX,
            y: event.clientY,
          })
          const current = gesture.current
          const geometry = imageGeometry()
          if (current === null || geometry === null) {
            return
          }
          if (current.kind === 'pan') {
            const point = pointers.current.get(event.pointerId)!
            const delta = {
              x: point.x - current.start.x,
              y: point.y - current.start.y,
            }
            if (Math.hypot(delta.x, delta.y) > ZOOM_DRAG_SLOP_PX) {
              zoomedAt.current = Date.now()
            }
            applyZoom(panBy(current.from, delta, geometry.size))
            return
          }
          const [a, b] = [...pointers.current.values()]
          const middle = { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 }
          const distance = Math.hypot(a.x - b.x, a.y - b.y)
          zoomedAt.current = Date.now()
          const scaled = zoomAt(
            current.from,
            (current.from.scale * distance) / current.distance,
            {
              x: current.middle.x - geometry.centre.x,
              y: current.middle.y - geometry.centre.y,
            },
            geometry.size,
          )
          // Two fingers that move together also pan.
          applyZoom(
            panBy(
              scaled,
              {
                x: middle.x - current.middle.x,
                y: middle.y - current.middle.y,
              },
              geometry.size,
            ),
          )
        }}
        onPointerUp={(event) => {
          if (pointers.current.delete(event.pointerId)) {
            beginGesture()
          }
        }}
        onPointerCancel={(event) => {
          if (pointers.current.delete(event.pointerId)) {
            beginGesture()
          }
        }}
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
              Date.now() - zoomedAt.current >= ZOOM_CLICK_GRACE_MS &&
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
        {hasImage && (
          /*
            Its own group under the toolbar, not more toolbar buttons: the
            toolbar is one row across the top, and on a 320px phone it already
            holds up to six 44px controls. Two more pushed it off the left edge
            and clipped Reply. A vertical pair on the right edge, as maps put
            their zoom, keeps every target 44px at any width.

            Never disabled at the limits: a disabled button drops focus to the
            body, out of the dialog's focus trap. A press at a limit is simply
            a no-op.
          */
          <div class="lightbox-zoom" role="group" aria-label="Zoom">
            <button
              type="button"
              class="ghost lightbox-action lightbox-zoom-in"
              aria-label="Zoom in"
              title="Zoom in (+)"
              onClick={() => zoomTo(zoomRef.current.scale * ZOOM_STEP)}
            >
              +
            </button>
            <button
              type="button"
              class="ghost lightbox-action lightbox-zoom-out"
              aria-label="Zoom out"
              title="Zoom out (-)"
              onClick={() => zoomTo(zoomRef.current.scale / ZOOM_STEP)}
            >
              −
            </button>
          </div>
        )}
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
          <LightboxZoomContext.Provider value={zoomContext}>
            {children}
          </LightboxZoomContext.Provider>
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
/**
 * How the open image ended up, for a caller deciding what controls to offer.
 *
 * ADR 0101 split the failure in two — `unsupported-format` kept Save,
 * `undecodable` withheld it — on the reasoning that unidentifiable bytes are
 * most likely the proxy's ciphertext-fallback 200. Tracing that path found it
 * near-unreachable (see `sniff.ts`): the proxy fails closed with a 404 while an
 * event is undecrypted, and once decrypted the key is *in* the event, so a
 * decrypt failure is a 502 and never bytes. Unidentifiable bytes are therefore
 * far more likely a format no table here carries — exactly the case where
 * downloading to open elsewhere is the remedy — so the split had no cases left
 * to separate and both now report as `failed` (#359).
 */
export type LightboxImageOutcome = 'pending' | 'displayed' | 'failed'

export function LightboxImage({
  accountId,
  media,
  onOutcome,
}: {
  accountId: string
  /**
   * The full descriptor, not just the url: naming *why* an image would not
   * decode needs the declared mimetype and the filename, and the alt text is
   * derived from the same fields both call sites were already deriving it from.
   */
  media: ParsedMedia
  /**
   * How the display attempt ended. A ready blob is not necessarily a picture —
   * a HEIC arrives intact and still will not paint — and `<img>` decode is the
   * only place that can be observed.
   */
  onOutcome?: (outcome: LightboxImageOutcome) => void
}) {
  // Retry generation — see `MediaRequestOptions.attempt`. Bumping it re-fetches
  // under a fresh object URL, which is what recovers the transient WebKit
  // decode failures that dominate on iOS (issue #359).
  const [attempt, setAttempt] = useState(0)
  const autoRetried = useRef(false)
  const { state, invalidate } = useMediaBlob(accountId, media.url, {
    eager: true,
    attempt,
  })
  // Bound to the object url the verdict was reached on — see `MediaImage`,
  // where the same shape stops a retry re-mounting the url that just failed.
  const [failure, setFailure] = useState<{
    url: string
    format?: SniffedFormat
  } | null>(null)
  const onOutcomeRef = useRef(onOutcome)
  useEffect(() => {
    onOutcomeRef.current = onOutcome
  })
  const zoom = useContext(LightboxZoomContext)
  const zoomRef = useRef(zoom)
  useEffect(() => {
    zoomRef.current = zoom
  })

  const alt = media.caption ?? media.filename

  const decodeFailed =
    failure !== null && (state.status !== 'ready' || state.url === failure.url)

  const retry = () => {
    autoRetried.current = true
    setAttempt((previous) => previous + 1)
    onOutcomeRef.current?.('pending')
  }

  // A new object means a fresh verdict — paging must not carry the previous
  // image's failure, or its success, onto the next one.
  useEffect(() => {
    autoRetried.current = false
    setFailure(null)
    setAttempt(0)
    onOutcomeRef.current?.('pending')
    // Paging to the next image starts it at fit, not at the last one's zoom.
    zoomRef.current?.reset()
  }, [media.url])

  return (
    <div tabindex={0} class="lightbox-image">
      {state.status === 'error' ? (
        // A failed fetch is its own outcome, and it carries Retry too — one
        // decode glitch plus one network glitch must not rebuild a dead end.
        // Except when the server reports the bytes as undecryptable, which is
        // terminal and gets no Retry (see `MediaImage`, and #361).
        <div class="lightbox-failure">
          <p class="muted placeholder">
            {state.error?.kind === 'undecryptable'
              ? 'Encrypted media — the server could not decrypt it'
              : 'Could not load image'}
          </p>
          {state.error?.kind !== 'undecryptable' && (
            <button type="button" class="ghost" onClick={retry}>
              Retry
            </button>
          )}
        </div>
      ) : decodeFailed ? (
        <div class="lightbox-failure">
          <p class="muted placeholder">
            {imageDecodeFailureMessage(media, failure?.format)}
          </p>
          <button type="button" class="ghost" onClick={retry}>
            Retry
          </button>
        </div>
      ) : state.status === 'ready' && state.url !== undefined ? (
        <img
          // Registering is what offers zoom: only a displayed image gets it.
          ref={zoom?.register}
          src={state.url}
          alt={alt}
          draggable={false}
          style={
            zoom?.transform === undefined
              ? undefined
              : { transform: zoom.transform }
          }
          onLoad={() => onOutcomeRef.current?.('displayed')}
          onError={() => {
            // Drop the failed bytes from the cache first — see `MediaImage`,
            // which explains why an entry outliving its holder would otherwise
            // serve the same broken object to the retry and to the next mount.
            invalidate()
            // The first failure buys a re-fetch, not a verdict.
            if (!autoRetried.current) {
              retry()
              return
            }
            if (state.url === undefined) {
              return
            }
            setFailure({ url: state.url, format: state.format })
            onOutcomeRef.current?.('failed')
          }}
        />
      ) : (
        <div class="media-skeleton" aria-hidden="true" />
      )}
    </div>
  )
}
