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
    onTouchMove,
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
      onTouchMove={onTouchMove}
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

  // iOS cancels the pointer when a native scroll starts, and only a
  // `touchmove` preventDefault stops that. Measured in the shell: normal-speed
  // reply swipes with a little upward drift all ended in `pointercancel` with
  // the timeline scrolled, badge shown and no reply.
  describe('claiming the touch from native scrolling', () => {
    function touchMove(target: Element, x: number, y: number): boolean {
      return fireEvent.touchMove(target, {
        touches: [{ clientX: x, clientY: y }],
      })
    }

    it('prevents the touchmove that locks a leftward swipe', () => {
      const view = render(<Harness eligible onAction={vi.fn()} />)
      const surface = view.getByTestId('surface')

      pointer(surface, 'down', 200)
      // Below the decision threshold: still free to become a scroll.
      expect(touchMove(surface, 195, 42)).toBe(true)
      // Leftward and mostly flat, touch first — claimed on this very move.
      expect(touchMove(surface, 170, 50)).toBe(false)
      pointer(surface, 'move', 170)
      // And for the rest of the gesture.
      expect(touchMove(surface, 120, 60)).toBe(false)
    })

    it('leaves a vertical drag to the native scroll', () => {
      const view = render(<Harness eligible onAction={vi.fn()} />)
      const surface = view.getByTestId('surface')

      pointer(surface, 'down', 200)
      expect(touchMove(surface, 196, 80)).toBe(true)
      expect(touchMove(surface, 190, 140)).toBe(true)
    })

    it('leaves a rightward drag to the room swipe-back', () => {
      const view = render(<Harness eligible onAction={vi.fn()} />)
      const surface = view.getByTestId('surface')

      pointer(surface, 'down', 200)
      expect(touchMove(surface, 240, 42)).toBe(true)
    })

    it('claims nothing on an ineligible surface', () => {
      const view = render(<Harness eligible={false} onAction={vi.fn()} />)
      const surface = view.getByTestId('surface')

      pointer(surface, 'down', 200)
      expect(touchMove(surface, 150, 42)).toBe(true)
    })
  })
})
