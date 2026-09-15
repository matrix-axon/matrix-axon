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
const MOBILE_BACK_PARALLAX = 0.18

export type MobileBackPresentation = {
  foreground: HTMLElement
  destination?: HTMLElement
}

type SwipeStart<T extends HTMLElement> = {
  x: number
  y: number
  presentation: MobileBackPresentation
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
 * Use the shell's already-mounted room list as the destination behind a page.
 * The fallback keeps page-level tests and any future non-shell mount usable.
 */
export function roomListBackPresentation(
  surface: HTMLElement,
  fallback: HTMLElement | null,
): MobileBackPresentation | null {
  const main = surface.closest('main')
  const shellBody = main?.parentElement
  const destination = shellBody?.querySelector<HTMLElement>(
    ':scope > #room-sidebar',
  )
  if (main !== null && destination !== null) {
    return { foreground: main, destination }
  }
  return fallback === null ? null : { foreground: fallback }
}

/**
 * The shared narrow-screen rightward pane gesture.
 *
 * The caller supplies the pane that should follow the finger and the action
 * that replaces it. Controls, horizontal scrollers, and iOS's native-back edge
 * band are always left alone.
 */
export function useMobileSwipeBack<T extends HTMLElement>({
  getPresentation,
  onAccepted,
  onBack,
}: {
  getPresentation: (surface: T) => MobileBackPresentation | null
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
  const presented = useRef<MobileBackPresentation | null>(null)
  const presentedSurface = useRef<T | null>(null)
  const resetTimer = useRef<number | null>(null)
  const destinationWasInert = useRef(false)

  const clearPresentation = () => {
    if (resetTimer.current !== null) {
      window.clearTimeout(resetTimer.current)
      resetTimer.current = null
    }
    const presentation = presented.current
    presentation?.foreground.classList.remove(
      'mobile-back-pane',
      'mobile-back-dragging',
      'mobile-back-settling',
    )
    presentation?.foreground.style.removeProperty('--mobile-back-offset')
    const destination = presentation?.destination
    destination?.classList.remove(
      'mobile-back-destination-active',
      'mobile-back-destination-dragging',
      'mobile-back-destination-settling',
    )
    destination?.style.removeProperty('--mobile-back-destination-offset')
    if (destination !== undefined) {
      destination.inert = destinationWasInert.current
    }
    presented.current = null
    const surface = presentedSurface.current
    surface?.classList.remove(
      'mobile-back-active',
      'mobile-back-dragging',
      'mobile-back-settling',
      'mobile-back-armed',
    )
    presentedSurface.current = null
  }

  const preview = (start: SwipeStart<T>, dx: number) => {
    const distance = Math.min(Math.max(dx, 0), window.innerWidth)
    const { foreground, destination } = start.presentation
    if (presented.current === null && destination !== undefined) {
      destinationWasInert.current = destination.inert === true
      destination.inert = true
    }
    presented.current = start.presentation
    presentedSurface.current = start.surface
    foreground.classList.add('mobile-back-pane', 'mobile-back-dragging')
    foreground.classList.remove('mobile-back-settling')
    foreground.style.setProperty('--mobile-back-offset', `${distance}px`)
    destination?.classList.add(
      'mobile-back-destination-active',
      'mobile-back-destination-dragging',
    )
    destination?.classList.remove('mobile-back-destination-settling')
    destination?.style.setProperty(
      '--mobile-back-destination-offset',
      `${-(window.innerWidth - distance) * MOBILE_BACK_PARALLAX}px`,
    )
    start.surface.classList.add('mobile-back-active', 'mobile-back-dragging')
    start.surface.classList.remove('mobile-back-settling')
    start.surface.classList.toggle('mobile-back-armed', dx >= SWIPE_MIN_X)
  }

  const settle = (start: SwipeStart<T>, accepted: boolean) => {
    const { foreground, destination } = start.presentation
    if (!foreground.classList.contains('mobile-back-dragging')) {
      return
    }
    foreground.classList.remove('mobile-back-dragging')
    foreground.classList.add('mobile-back-settling')
    foreground.style.setProperty(
      '--mobile-back-offset',
      accepted ? `${window.innerWidth}px` : '0px',
    )
    destination?.classList.remove('mobile-back-destination-dragging')
    destination?.classList.add('mobile-back-destination-settling')
    destination?.style.setProperty(
      '--mobile-back-destination-offset',
      accepted ? '0px' : `${-window.innerWidth * MOBILE_BACK_PARALLAX}px`,
    )
    start.surface.classList.remove('mobile-back-dragging', 'mobile-back-armed')
    start.surface.classList.add('mobile-back-settling')
    if (resetTimer.current !== null) {
      window.clearTimeout(resetTimer.current)
    }
    const delay = window.matchMedia('(prefers-reduced-motion: reduce)').matches
      ? 0
      : MOBILE_BACK_SETTLE_MS
    resetTimer.current = window.setTimeout(() => {
      if (accepted) {
        onBack()
      }
      clearPresentation()
    }, delay)
  }

  useEffect(
    () => () => {
      clearPresentation()
    },
    [],
  )

  const onTouchStart = (event: JSX.TargetedTouchEvent<T>) => {
    clearPresentation()
    swipeLocked.current = false
    if (
      !window.matchMedia(SINGLE_PANE_QUERY).matches ||
      event.touches.length !== 1 ||
      isGestureControl(event.target) ||
      isHorizontallyScrollable(event.target)
    ) {
      swipeStart.current = null
      return
    }
    const touch = event.touches[0]
    const presentation = getPresentation(event.currentTarget)
    if (touch.clientX < NATIVE_BACK_EDGE_PX || presentation === null) {
      swipeStart.current = null
      return
    }
    swipeStart.current = {
      x: touch.clientX,
      y: touch.clientY,
      presentation,
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
      settle(start, false)
      return
    }
    onAccepted?.(dx, dy)
    if (
      !start.presentation.foreground.classList.contains('mobile-back-dragging')
    ) {
      preview(start, dx)
    }
    settle(start, true)
  }

  const onTouchCancel = () => {
    const start = swipeStart.current
    swipeStart.current = null
    swipeLocked.current = false
    if (start !== null) {
      settle(start, false)
    }
  }

  return { onTouchStart, onTouchMove, onTouchEnd, onTouchCancel }
}
