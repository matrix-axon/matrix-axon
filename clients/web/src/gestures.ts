/**
 * Touch-gesture constants shared by every horizontal swipe surface.
 *
 * The general thresholds drive message actions and the media viewer's swipe
 * paging (ADR 0081). Interactive back navigation has a shorter completion
 * threshold because its destination is already visible and a short drag can
 * visibly cancel back into the current pane.
 */

/** Minimum horizontal travel before a message or media swipe counts. */
export const SWIPE_MIN_X = 72
/** Minimum horizontal travel before an interactive back swipe completes. */
export const SWIPE_BACK_MIN_X = 48
/** Maximum vertical drift a horizontal swipe may accumulate. */
export const SWIPE_MAX_Y = 64
/** How much more horizontal than vertical the travel must be. */
export const SWIPE_AXIS_RATIO = 1.4
/**
 * How far a touch has to travel before we commit to a direction: small enough
 * to claim the gesture early, large enough not to mistake a tap's jitter for
 * a swipe.
 */
export const SWIPE_DECISION_THRESHOLD = 10
/** Hold duration shared by message surfaces and their action controls. */
export const MESSAGE_TOUCH_HOLD_MS = 550
/** Settle duration shared by message and pane swipe animations. */
export const GESTURE_SWIPE_SETTLE_MS = 180
/**
 * Minimum vertical travel for a downward dismiss. Deliberately longer than
 * the horizontal minimum: paging is cheap to undo, closing the viewer is not,
 * so it should take a more committed gesture.
 */
export const SWIPE_MIN_Y = 96
/**
 * A band along the left edge belongs to the browser's own swipe-back, which we
 * cannot pre-empt: WebKit's is a UIKit recognizer on the scroll view, not a
 * cancelable touch default, so `preventDefault` does not call it off. Racing
 * it is worse than losing to it (ADR 0075).
 *
 * This applies to the lightbox too. The recognizer is live over a fullscreen
 * overlay — being on top in z-order does not take a gesture away from UIKit —
 * so the viewer declines the same band rather than fighting for it.
 */
export const NATIVE_BACK_EDGE_PX = 30

export interface SwipeStart {
  x: number
  y: number
}

/** Whether a gesture began on an interactive control that owns the input. */
export function isGestureControlTarget(target: EventTarget | null): boolean {
  return (
    target instanceof Element &&
    target.closest(
      'a, button, input, textarea, select, summary, [contenteditable="true"], [role="button"], [role="textbox"], emoji-picker',
    ) !== null
  )
}

/**
 * Whether `target` sits inside content that pans horizontally on its own, such
 * as a wide code block or table. Room and row swipe recognizers both yield to
 * it so the same touch cannot mean content pan in one layer and an action in
 * another. Walking stops at the enclosing swipe-back surface.
 */
export function isHorizontallyScrollable(target: EventTarget | null): boolean {
  let element = target instanceof Element ? target : null
  while (
    element !== null &&
    !element.classList.contains('mobile-back-surface')
  ) {
    if (
      element instanceof HTMLElement &&
      element.scrollWidth > element.clientWidth &&
      /(auto|scroll)/.test(getComputedStyle(element).overflowX)
    ) {
      return true
    }
    element = element.parentElement
  }
  return false
}

/**
 * Whether a completed touch counts as a swipe, and in which direction. `null`
 * means it does not qualify.
 *
 * Horizontal and vertical are tested with mirrored rules — the same
 * cross-axis tolerance and axis ratio each way — so a diagonal drag resolves
 * to nothing rather than to whichever axis happened to be checked first.
 */
export function swipeDirection(
  start: SwipeStart,
  endX: number,
  endY: number,
): 'left' | 'right' | 'up' | 'down' | null {
  const dx = endX - start.x
  const dy = endY - start.y
  const absX = Math.abs(dx)
  const absY = Math.abs(dy)

  if (
    absX >= SWIPE_MIN_X &&
    absY <= SWIPE_MAX_Y &&
    absX >= absY * SWIPE_AXIS_RATIO
  ) {
    return dx > 0 ? 'right' : 'left'
  }
  if (
    absY >= SWIPE_MIN_Y &&
    absX <= SWIPE_MAX_Y &&
    absY >= absX * SWIPE_AXIS_RATIO
  ) {
    return dy > 0 ? 'down' : 'up'
  }
  return null
}
