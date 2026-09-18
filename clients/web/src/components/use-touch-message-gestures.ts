import { useCallback, useEffect, useRef, useState } from 'preact/hooks'
import {
  GESTURE_SWIPE_SETTLE_MS,
  isGestureControlTarget,
  isHorizontallyScrollable,
  MESSAGE_TOUCH_HOLD_MS,
  SWIPE_AXIS_RATIO,
  SWIPE_DECISION_THRESHOLD,
  SWIPE_MIN_X,
  swipeDirection,
} from '../gestures'
import type {
  MessageGestureAction,
  MessageGesturePreferences,
} from '../stores/message-gestures'

/** Finger travel that still counts as a tap, matching the room list. */
const ACTION_ROW_TAP_SLOP_PX = 10
const MESSAGE_DOUBLE_TAP_MS = 300
const MESSAGE_DOUBLE_TAP_SLOP_PX = 24
const MESSAGE_SWIPE_MAX_X = 96

function closestButton(
  target: EventTarget | null,
  selector: string | null,
): HTMLButtonElement | null {
  if (selector === null || !(target instanceof Element)) return null
  const control = target.closest(selector)
  return control instanceof HTMLButtonElement ? control : null
}

function isInlineLink(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest('a') !== null
}

function isTimestampControl(target: EventTarget | null): boolean {
  return (
    target instanceof Element && target.closest('.event-time-copy') !== null
  )
}

/**
 * The shared touch recognizer for a message row or one loaded gallery tile.
 *
 * It owns only gesture arbitration and swipe presentation. The caller owns
 * action eligibility and rendering, which lets a gallery identify the exact
 * event without teaching the recognizer about either timeline shape.
 */
export function useTouchMessageGestures<T extends HTMLElement>({
  eligible,
  preferences,
  openControlSelector,
  allowInlineLinks = false,
  onAction,
  onSingleTap,
  onTouchStart,
}: {
  eligible: boolean
  preferences: MessageGesturePreferences | null
  /** A tap on this control is delayed so a second tap can claim the gesture. */
  openControlSelector: string | null
  allowInlineLinks?: boolean
  onAction: (action: MessageGestureAction) => void
  onSingleTap: () => void
  onTouchStart?: () => void
}) {
  const surfaceRef = useRef<T>(null)
  const pointer = useRef<{
    pointerId: number
    startX: number
    startY: number
    direction: 'none' | 'left' | 'right' | 'vertical'
    held: boolean
    linkOnly: boolean
    openControl: HTMLButtonElement | null
    swipeAction: MessageGestureAction | null
  } | null>(null)
  const holdTimer = useRef<number | null>(null)
  const pendingTap = useRef<{
    x: number
    y: number
    at: number
    timer: number
  } | null>(null)
  const suppressNextClick = useRef(false)
  const programmaticOpen = useRef(false)
  const revealTimer = useRef<number | null>(null)
  const lastPointerType = useRef<string | null>(null)
  const [swipeReveal, setSwipeReveal] = useState<MessageGestureAction | null>(
    null,
  )
  const [swipeArmed, setSwipeArmed] = useState(false)
  const [swipeSettling, setSwipeSettling] = useState(false)
  const touchHoldEnabled =
    eligible && preferences?.bindings.touch_and_hold !== null

  const clearHoldTimer = useCallback(() => {
    if (holdTimer.current !== null) {
      window.clearTimeout(holdTimer.current)
      holdTimer.current = null
    }
  }, [])

  const cancelPendingTap = useCallback(() => {
    if (pendingTap.current !== null) {
      window.clearTimeout(pendingTap.current.timer)
      pendingTap.current = null
    }
  }, [])

  const clearSwipePresentation = useCallback(() => {
    if (revealTimer.current !== null) window.clearTimeout(revealTimer.current)
    revealTimer.current = null
    const surface = surfaceRef.current
    surface?.style.removeProperty('--message-swipe-offset')
    surface?.style.removeProperty('--message-swipe-reveal-width')
    surface?.classList.remove('gesture-swipe-settling')
    setSwipeReveal(null)
    setSwipeArmed(false)
    setSwipeSettling(false)
  }, [])

  const previewSwipeAction = (action: MessageGestureAction, dx: number) => {
    const distance = Math.min(Math.max(-dx, 0), MESSAGE_SWIPE_MAX_X)
    if (revealTimer.current !== null) {
      window.clearTimeout(revealTimer.current)
      revealTimer.current = null
    }
    const surface = surfaceRef.current
    surface?.style.setProperty('--message-swipe-offset', `${-distance}px`)
    surface?.style.setProperty('--message-swipe-reveal-width', `${distance}px`)
    setSwipeReveal(action)
    setSwipeArmed(-dx >= SWIPE_MIN_X)
    setSwipeSettling(false)
  }

  const settleSwipeAction = useCallback(() => {
    const surface = surfaceRef.current
    if (surface === null || swipeReveal === null) return
    surface.classList.add('gesture-swipe-settling')
    surface.style.setProperty('--message-swipe-offset', '0px')
    surface.style.setProperty('--message-swipe-reveal-width', '0px')
    setSwipeArmed(false)
    setSwipeSettling(true)
    if (revealTimer.current !== null) window.clearTimeout(revealTimer.current)
    revealTimer.current = window.setTimeout(() => {
      clearSwipePresentation()
      revealTimer.current = null
    }, GESTURE_SWIPE_SETTLE_MS)
  }, [clearSwipePresentation, swipeReveal])

  const cancelGesture = useCallback(() => {
    surfaceRef.current?.classList.remove('touch-gesture-active')
    clearHoldTimer()
    cancelPendingTap()
    pointer.current = null
    settleSwipeAction()
  }, [cancelPendingTap, clearHoldTimer, settleSwipeAction])

  const finishTap = (x: number, y: number, singleTap: () => void): void => {
    const action = eligible ? (preferences?.bindings.double_tap ?? null) : null
    if (action === null) {
      singleTap()
      return
    }
    const now = performance.now()
    const first = pendingTap.current
    if (
      first !== null &&
      now - first.at <= MESSAGE_DOUBLE_TAP_MS &&
      Math.hypot(x - first.x, y - first.y) <= MESSAGE_DOUBLE_TAP_SLOP_PX
    ) {
      window.clearTimeout(first.timer)
      pendingTap.current = null
      onAction(action)
      return
    }
    if (first !== null) window.clearTimeout(first.timer)
    const timer = window.setTimeout(() => {
      pendingTap.current = null
      singleTap()
    }, MESSAGE_DOUBLE_TAP_MS)
    pendingTap.current = { x, y, at: now, timer }
  }

  useEffect(() => {
    if (!eligible) cancelGesture()
  }, [cancelGesture, eligible])

  useEffect(
    () => () => {
      clearHoldTimer()
      cancelPendingTap()
      if (revealTimer.current !== null) window.clearTimeout(revealTimer.current)
    },
    [cancelPendingTap, clearHoldTimer],
  )

  return {
    surfaceRef,
    lastPointerType,
    suppressNextClick,
    touchHoldEnabled,
    swipeReveal,
    swipeArmed,
    swipeSettling,
    onPointerDown: (pointerEvent: PointerEvent) => {
      lastPointerType.current = pointerEvent.pointerType
      if (pointerEvent.pointerType === 'mouse') {
        clearHoldTimer()
        pointer.current = null
        suppressNextClick.current = false
        programmaticOpen.current = false
        surfaceRef.current?.classList.remove('touch-gesture-active')
        return
      }
      onTouchStart?.()
      if (pendingTap.current !== null) {
        window.clearTimeout(pendingTap.current.timer)
      }
      if (
        pointer.current !== null &&
        pointer.current.pointerId !== pointerEvent.pointerId
      ) {
        cancelGesture()
        return
      }
      clearHoldTimer()
      pointer.current = null
      suppressNextClick.current = false
      clearSwipePresentation()
      const linkOnly = allowInlineLinks && isInlineLink(pointerEvent.target)
      const openControl = closestButton(
        pointerEvent.target,
        openControlSelector,
      )
      if (
        isGestureControlTarget(pointerEvent.target) &&
        !linkOnly &&
        openControl === null
      )
        return
      if (isHorizontallyScrollable(pointerEvent.target)) return
      if (linkOnly && !touchHoldEnabled) return
      if (touchHoldEnabled) {
        surfaceRef.current?.classList.add('touch-gesture-active')
      }
      pointer.current = {
        pointerId: pointerEvent.pointerId,
        startX: pointerEvent.clientX,
        startY: pointerEvent.clientY,
        direction: 'none',
        held: false,
        linkOnly,
        openControl,
        swipeAction:
          eligible && !linkOnly
            ? (preferences?.bindings.swipe_left ?? null)
            : null,
      }
      const holdAction = eligible
        ? (preferences?.bindings.touch_and_hold ?? null)
        : null
      if (holdAction !== null) {
        holdTimer.current = window.setTimeout(() => {
          const gesture = pointer.current
          if (gesture === null || gesture.direction !== 'none') return
          cancelPendingTap()
          gesture.held = true
          suppressNextClick.current = true
          onAction(holdAction)
        }, MESSAGE_TOUCH_HOLD_MS)
      }
    },
    onPointerMove: (pointerEvent: PointerEvent) => {
      const gesture = pointer.current
      if (gesture === null || gesture.pointerId !== pointerEvent.pointerId) {
        return
      }
      const dx = pointerEvent.clientX - gesture.startX
      const dy = pointerEvent.clientY - gesture.startY
      if (gesture.direction === 'left') {
        if (gesture.swipeAction !== null) {
          previewSwipeAction(gesture.swipeAction, dx)
          pointerEvent.preventDefault()
        }
        return
      }
      if (gesture.direction !== 'none') return
      const absX = Math.abs(dx)
      const absY = Math.abs(dy)
      if (absX < SWIPE_DECISION_THRESHOLD && absY < SWIPE_DECISION_THRESHOLD)
        return
      clearHoldTimer()
      cancelPendingTap()
      if (absX >= absY * SWIPE_AXIS_RATIO) {
        gesture.direction = dx < 0 ? 'left' : 'right'
        if (dx < 0 && gesture.swipeAction !== null) {
          previewSwipeAction(gesture.swipeAction, dx)
          pointerEvent.preventDefault()
        }
      } else {
        gesture.direction = 'vertical'
      }
    },
    onPointerCancel: cancelGesture,
    onPointerUp: (pointerEvent: PointerEvent) => {
      surfaceRef.current?.classList.remove('touch-gesture-active')
      clearHoldTimer()
      const gesture = pointer.current
      pointer.current = null
      if (
        pointerEvent.pointerType === 'mouse' ||
        gesture === null ||
        gesture.pointerId !== pointerEvent.pointerId
      ) {
        return
      }
      if (gesture.held) {
        pointerEvent.preventDefault()
        return
      }
      if (gesture.linkOnly) {
        if (
          gesture.direction !== 'none' ||
          Math.hypot(
            pointerEvent.clientX - gesture.startX,
            pointerEvent.clientY - gesture.startY,
          ) > ACTION_ROW_TAP_SLOP_PX
        ) {
          suppressNextClick.current = true
        }
        return
      }
      if (gesture.direction === 'left' && eligible) {
        suppressNextClick.current = true
        settleSwipeAction()
        if (
          swipeDirection(
            { x: gesture.startX, y: gesture.startY },
            pointerEvent.clientX,
            pointerEvent.clientY,
          ) === 'left'
        ) {
          const action = gesture.swipeAction
          if (action !== null) {
            pointerEvent.preventDefault()
            onAction(action)
          }
        }
        return
      }
      if (
        gesture.direction !== 'none' ||
        Math.hypot(
          pointerEvent.clientX - gesture.startX,
          pointerEvent.clientY - gesture.startY,
        ) > ACTION_ROW_TAP_SLOP_PX
      ) {
        suppressNextClick.current = true
        return
      }
      pointerEvent.preventDefault()
      suppressNextClick.current = true
      finishTap(
        pointerEvent.clientX,
        pointerEvent.clientY,
        gesture.openControl === null
          ? onSingleTap
          : () => {
              if (!gesture.openControl?.isConnected) return
              programmaticOpen.current = true
              gesture.openControl.click()
            },
      )
    },
    onContextMenu: (event: MouseEvent) => {
      if (
        touchHoldEnabled &&
        pointer.current !== null &&
        !isTimestampControl(event.target)
      )
        event.preventDefault()
    },
    onClickCapture: (click: MouseEvent) => {
      if (programmaticOpen.current) {
        programmaticOpen.current = false
        return
      }
      if (
        suppressNextClick.current &&
        closestButton(click.target, openControlSelector) !== null
      ) {
        suppressNextClick.current = false
        click.preventDefault()
        click.stopPropagation()
      }
    },
  }
}
