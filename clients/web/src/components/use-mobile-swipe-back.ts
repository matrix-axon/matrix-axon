import type { JSX } from 'preact'
import { useEffect, useRef } from 'preact/hooks'
import {
  isHorizontallyScrollable,
  NATIVE_BACK_EDGE_PX,
  SWIPE_AXIS_RATIO,
  SWIPE_DECISION_THRESHOLD,
  SWIPE_MAX_Y,
  SWIPE_MIN_X,
} from '../gestures'
import { SINGLE_PANE_QUERY } from '../layout'

const MOBILE_BACK_SETTLE_MS = 180

type SwipeStart<T extends HTMLElement> = {
  x: number
  y: number
  pane: HTMLElement
  surface: T
}

function isGestureControl(target: EventTarget | null): boolean {
  return (
    target instanceof Element &&
    target.closest(
      'a, button, input, textarea, select, summary, [contenteditable="true"], [role="button"], [role="textbox"], emoji-picker',
    ) !== null
  )
}

/**
 * The shared narrow-screen rightward pane gesture.
 *
 * The caller supplies the pane that should follow the finger and the action
 * that replaces it. Controls, horizontal scrollers, and iOS's native-back edge
 * band are always left alone.
 */
export function useMobileSwipeBack<T extends HTMLElement>({
  getPane,
  onAccepted,
  onBack,
}: {
  getPane: (surface: T) => HTMLElement | null
  onAccepted?: (dx: number, dy: number) => void
  onBack: () => void
}): {
  onTouchStart: (event: JSX.TargetedTouchEvent<T>) => void
  onTouchMove: (event: JSX.TargetedTouchEvent<T>) => void
  onTouchEnd: (event: JSX.TargetedTouchEvent<T>) => void
  onTouchCancel: () => void
} {
  const swipeStart = useRef<SwipeStart<T> | null>(null)
  const swipeLocked = useRef(false)
  const presentedPane = useRef<HTMLElement | null>(null)
  const presentedSurface = useRef<T | null>(null)
  const resetTimer = useRef<number | null>(null)

  const clearPresentation = () => {
    if (resetTimer.current !== null) {
      window.clearTimeout(resetTimer.current)
      resetTimer.current = null
    }
    const pane = presentedPane.current
    pane?.classList.remove('mobile-back-dragging', 'mobile-back-settling')
    pane?.style.removeProperty('--mobile-back-offset')
    presentedPane.current = null
    const surface = presentedSurface.current
    surface?.classList.remove(
      'mobile-back-active',
      'mobile-back-dragging',
      'mobile-back-settling',
      'mobile-back-armed',
    )
    surface?.style.removeProperty('--mobile-back-reveal-width')
    presentedSurface.current = null
  }

  const preview = (start: SwipeStart<T>, dx: number) => {
    const distance = Math.min(Math.max(dx, 0), window.innerWidth)
    presentedPane.current = start.pane
    presentedSurface.current = start.surface
    start.pane.classList.add('mobile-back-dragging')
    start.pane.classList.remove('mobile-back-settling')
    start.pane.style.setProperty('--mobile-back-offset', `${distance}px`)
    start.surface.classList.add('mobile-back-active', 'mobile-back-dragging')
    start.surface.classList.remove('mobile-back-settling')
    start.surface.classList.toggle('mobile-back-armed', dx >= SWIPE_MIN_X)
    start.surface.style.setProperty(
      '--mobile-back-reveal-width',
      `${distance}px`,
    )
  }

  const settle = (start: SwipeStart<T>) => {
    if (!start.pane.classList.contains('mobile-back-dragging')) {
      return
    }
    start.pane.classList.remove('mobile-back-dragging')
    start.pane.classList.add('mobile-back-settling')
    start.pane.style.setProperty('--mobile-back-offset', '0px')
    start.surface.classList.remove('mobile-back-dragging', 'mobile-back-armed')
    start.surface.classList.add('mobile-back-settling')
    start.surface.style.setProperty('--mobile-back-reveal-width', '0px')
    if (resetTimer.current !== null) {
      window.clearTimeout(resetTimer.current)
    }
    resetTimer.current = window.setTimeout(
      clearPresentation,
      MOBILE_BACK_SETTLE_MS,
    )
  }

  useEffect(
    () => () => {
      if (resetTimer.current !== null) {
        window.clearTimeout(resetTimer.current)
      }
    },
    [],
  )

  const onTouchStart = (event: JSX.TargetedTouchEvent<T>) => {
    clearPresentation()
    swipeLocked.current = false
    const touch = event.touches[0]
    const pane = getPane(event.currentTarget)
    if (
      !window.matchMedia(SINGLE_PANE_QUERY).matches ||
      event.touches.length !== 1 ||
      touch.clientX < NATIVE_BACK_EDGE_PX ||
      isGestureControl(event.target) ||
      isHorizontallyScrollable(event.target) ||
      pane === null
    ) {
      swipeStart.current = null
      return
    }
    swipeStart.current = {
      x: touch.clientX,
      y: touch.clientY,
      pane,
      surface: event.currentTarget,
    }
  }

  const onTouchMove = (event: JSX.TargetedTouchEvent<T>) => {
    const start = swipeStart.current
    if (start === null || event.touches.length !== 1) {
      return
    }
    const touch = event.touches[0]
    const dx = touch.clientX - start.x
    const dy = touch.clientY - start.y
    if (swipeLocked.current) {
      preview(start, dx)
      event.preventDefault()
      return
    }
    const absX = Math.abs(dx)
    const absY = Math.abs(dy)
    if (absX < SWIPE_DECISION_THRESHOLD && absY < SWIPE_DECISION_THRESHOLD) {
      return
    }
    if (dx > 0 && absX > absY * SWIPE_AXIS_RATIO) {
      swipeLocked.current = true
      preview(start, dx)
      event.preventDefault()
    } else {
      swipeStart.current = null
    }
  }

  const onTouchEnd = (event: JSX.TargetedTouchEvent<T>) => {
    const start = swipeStart.current
    swipeStart.current = null
    swipeLocked.current = false
    if (
      start === null ||
      !window.matchMedia(SINGLE_PANE_QUERY).matches ||
      event.changedTouches.length === 0
    ) {
      return
    }
    const touch = event.changedTouches[0]
    const dx = touch.clientX - start.x
    const dy = touch.clientY - start.y
    const absY = Math.abs(dy)
    if (
      dx < SWIPE_MIN_X ||
      absY > SWIPE_MAX_Y ||
      dx < absY * SWIPE_AXIS_RATIO
    ) {
      settle(start)
      return
    }
    onAccepted?.(dx, dy)
    settle(start)
    onBack()
  }

  const onTouchCancel = () => {
    const start = swipeStart.current
    swipeStart.current = null
    swipeLocked.current = false
    if (start !== null) {
      settle(start)
    }
  }

  return { onTouchStart, onTouchMove, onTouchEnd, onTouchCancel }
}
