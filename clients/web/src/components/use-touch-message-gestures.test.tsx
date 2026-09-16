import { act, cleanup, fireEvent, render } from '@testing-library/preact'
import { afterEach, describe, expect, it, vi } from 'vitest'
import {
  defaultMessageGestures,
  type MessageGestureAction,
} from '../stores/message-gestures'
import { useTouchMessageGestures } from './use-touch-message-gestures'

function Harness({
  eligible,
  onAction,
}: {
  eligible: boolean
  onAction: (action: MessageGestureAction) => void
}) {
  const gestures = useTouchMessageGestures<HTMLDivElement>({
    eligible,
    preferences: defaultMessageGestures(),
    openControlSelector: null,
    onAction,
    onSingleTap: () => {},
  })
  const {
    surfaceRef,
    swipeReveal,
    onPointerDown,
    onPointerMove,
    onPointerCancel,
    onPointerUp,
  } = gestures
  return (
    <div
      data-testid="surface"
      ref={surfaceRef}
      class={swipeReveal === null ? '' : 'gesture-swipe-reveal'}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerCancel={onPointerCancel}
      onPointerUp={onPointerUp}
    />
  )
}

function pointer(target: Element, kind: 'down' | 'move' | 'up', x: number) {
  const init = {
    pointerType: 'touch',
    pointerId: 1,
    clientX: x,
    clientY: 40,
  }
  if (kind === 'down') fireEvent.pointerDown(target, init)
  else if (kind === 'move') fireEvent.pointerMove(target, init)
  else fireEvent.pointerUp(target, init)
}

afterEach(() => {
  cleanup()
  vi.useRealTimers()
})

describe('useTouchMessageGestures', () => {
  it('cancels pending gesture state when the surface becomes ineligible', () => {
    vi.useFakeTimers()
    const onAction = vi.fn()
    const view = render(<Harness eligible onAction={onAction} />)
    const surface = view.getByTestId('surface')

    pointer(surface, 'down', 130)
    pointer(surface, 'move', 90)
    expect(surface.classList.contains('gesture-swipe-reveal')).toBe(true)

    view.rerender(<Harness eligible={false} onAction={onAction} />)
    act(() => {
      vi.advanceTimersByTime(550)
    })

    expect(surface.classList.contains('gesture-swipe-reveal')).toBe(false)
    expect(surface.style.getPropertyValue('--message-swipe-offset')).toBe('')
    expect(onAction).not.toHaveBeenCalled()
  })
})
