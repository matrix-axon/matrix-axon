import { useEffect, useRef } from 'preact/hooks'
import type { JSX } from 'preact'
import { nativeBackEdgePx, swipeDirection, type SwipeStart } from '../gestures'

/**
 * How long after a committed swipe a synthesised `click` is still assumed to
 * belong to that swipe rather than to a fresh tap.
 */
const SWIPE_CLICK_GRACE_MS = 400

/**
 * Swipe-to-page for the media viewer (ADR 0081). Right reveals the older
 * image, left the newer — the direction the content moves, matching the
 * room's swipe-back and every native photo viewer.
 *
 * Bound on the overlay itself. There is no collision with the room's
 * swipe-back handlers even though those sit on `.room-body`: the lightbox
 * portals to `document.body`, and Preact portals do not re-dispatch DOM
 * events through the virtual tree, so a touch inside the dialog never reaches
 * the timeline's handlers. That is asserted end-to-end rather than defended
 * with a guard here.
 *
 * The left edge band is still declined in a browser — see
 * `NATIVE_BACK_EDGE_PX`; WebKit's back recognizer is live over a fullscreen
 * overlay. The native shell has no such recognizer and declines nothing.
 */
export function useSwipePaging(handlers: {
  onOlder: () => void
  onNewer: () => void
  /**
   * Swipe down to dismiss. Only fires for a gesture that *started on the
   * image* — the same overlay hosts a PDF (ADR 0072) whose own vertical
   * scrolling would otherwise close the viewer under the reader.
   */
  onDismiss?: () => void
  /**
   * Whether to leave this touch alone. A zoomed image (`image-zoom.ts`) uses a
   * one-finger drag to pan, so it must not also page or dismiss.
   */
  blocked?: () => boolean
}): {
  onTouchStart: JSX.TouchEventHandler<HTMLElement>
  onTouchEnd: JSX.TouchEventHandler<HTMLElement>
  onTouchCancel: JSX.TouchEventHandler<HTMLElement>
  /**
   * Whether a swipe was just committed. A browser still synthesises a `click`
   * after a touch that moved, so the tap-to-toggle-chrome gesture has to be
   * able to tell "the user tapped" from "the user swiped and a click followed".
   */
  swipedRecently: () => boolean
} {
  const start = useRef<SwipeStart | null>(null)
  const swipedAt = useRef(0)
  /** Whether this gesture began on the image, gating the dismiss. */
  const fromImage = useRef(false)
  // Read through a ref so a caller can pass inline closures without the
  // handlers changing identity on every render. Written in an effect, never
  // during render.
  const handlersRef = useRef(handlers)
  useEffect(() => {
    handlersRef.current = handlers
  })

  return {
    onTouchStart: (event) => {
      const touch = event.touches[0]
      if (
        event.touches.length !== 1 ||
        touch === undefined ||
        touch.clientX < nativeBackEdgePx() ||
        handlersRef.current.blocked?.() === true
      ) {
        start.current = null
        return
      }
      start.current = { x: touch.clientX, y: touch.clientY }
      fromImage.current =
        touch.target instanceof Element &&
        touch.target.closest('.lightbox-image') !== null
    },
    onTouchEnd: (event) => {
      const from = start.current
      start.current = null
      const touch = event.changedTouches[0]
      if (from === null || touch === undefined) {
        return
      }
      const direction = swipeDirection(from, touch.clientX, touch.clientY)
      if (direction === 'right') {
        swipedAt.current = Date.now()
        handlersRef.current.onOlder()
      } else if (direction === 'left') {
        swipedAt.current = Date.now()
        handlersRef.current.onNewer()
      } else if (direction === 'down' && fromImage.current) {
        // Marked as a swipe too, so the click the browser synthesises after
        // the touch cannot also toggle the chrome on the way out.
        swipedAt.current = Date.now()
        handlersRef.current.onDismiss?.()
      }
    },
    onTouchCancel: () => {
      start.current = null
      fromImage.current = false
    },
    swipedRecently: () => Date.now() - swipedAt.current < SWIPE_CLICK_GRACE_MS,
  }
}
