import { describe, expect, it } from 'vitest'
import {
  NATIVE_BACK_EDGE_PX,
  SWIPE_AXIS_RATIO,
  SWIPE_BACK_MIN_X,
  SWIPE_DECISION_THRESHOLD,
  SWIPE_MAX_Y,
  SWIPE_MIN_X,
  SWIPE_MIN_Y,
  swipeDirection,
} from './gestures'

/** From the origin, so a case reads as the travel it describes. */
const swipe = (dx: number, dy: number) => swipeDirection({ x: 0, y: 0 }, dx, dy)

describe('swipeDirection', () => {
  describe('horizontal', () => {
    it('resolves a clean drag each way', () => {
      expect(swipe(SWIPE_MIN_X, 0)).toBe('right')
      expect(swipe(-SWIPE_MIN_X, 0)).toBe('left')
    })

    it('declines a drag that is long enough but too steep', () => {
      // Past the horizontal minimum, but the vertical drift is over budget.
      expect(swipe(SWIPE_MIN_X, SWIPE_MAX_Y + 1)).toBeNull()
    })

    it('declines a drag that is not decisively horizontal', () => {
      // Inside the cross-axis budget, but not `SWIPE_AXIS_RATIO` times it.
      const dy = Math.ceil(SWIPE_MIN_X / SWIPE_AXIS_RATIO) + 1
      expect(dy).toBeLessThanOrEqual(SWIPE_MAX_Y)
      expect(swipe(SWIPE_MIN_X, dy)).toBeNull()
    })

    it('is exclusive at its own boundary', () => {
      expect(swipe(SWIPE_MIN_X - 1, 0)).toBeNull()
    })
  })

  describe('vertical', () => {
    it('resolves a clean drag each way', () => {
      expect(swipe(0, SWIPE_MIN_Y)).toBe('down')
      expect(swipe(0, -SWIPE_MIN_Y)).toBe('up')
    })

    it('declines a drag that is long enough but too wide', () => {
      expect(swipe(SWIPE_MAX_Y + 1, SWIPE_MIN_Y)).toBeNull()
    })

    it('takes the widest drift the budget allows, and no more', () => {
      // Note for whoever tunes these next: on this branch the axis ratio is
      // unreachable. Failing it needs `absX > SWIPE_MIN_Y / SWIPE_AXIS_RATIO`
      // (~68.6), which is already past `SWIPE_MAX_Y`, so the cross-axis budget
      // always decides first. The horizontal branch is not like this — there
      // the ratio genuinely binds, which the test above pins. Raising
      // `SWIPE_MAX_Y` past that figure would bring this ratio to life.
      expect(SWIPE_MIN_Y / SWIPE_AXIS_RATIO).toBeGreaterThan(SWIPE_MAX_Y)
      expect(swipe(SWIPE_MAX_Y, SWIPE_MIN_Y)).toBe('down')
      expect(swipe(SWIPE_MAX_Y + 1, SWIPE_MIN_Y)).toBeNull()
    })

    it('is exclusive at its own boundary', () => {
      expect(swipe(0, SWIPE_MIN_Y - 1)).toBeNull()
    })

    it('needs a more committed gesture than a page does', () => {
      // Closing the viewer is expensive to undo; paging is not. A drag that
      // would page horizontally must not dismiss vertically.
      expect(swipe(0, SWIPE_MIN_X)).toBeNull()
      expect(SWIPE_MIN_Y).toBeGreaterThan(SWIPE_MIN_X)
    })
  })

  describe('the two axes are mirrored', () => {
    it('resolves a diagonal to nothing rather than to either axis', () => {
      // The hazard the mirrored rules exist to prevent: a drag long enough on
      // both axes resolving to whichever was tested first. Checked in all four
      // quadrants, because a sign error shows up in exactly one.
      const long = Math.max(SWIPE_MIN_X, SWIPE_MIN_Y) + 20
      for (const sx of [1, -1]) {
        for (const sy of [1, -1]) {
          expect(swipe(sx * long, sy * long)).toBeNull()
        }
      }
    })

    it('applies the same cross-axis budget both ways', () => {
      // Same travel, axes swapped: neither qualifies, so the tolerance cannot
      // be laxer in one direction than the other.
      const long = SWIPE_MIN_Y
      const drift = SWIPE_MAX_Y + 1
      expect(swipe(long, drift)).toBeNull()
      expect(swipe(drift, long)).toBeNull()
    })
  })

  it('resolves a stationary touch to nothing', () => {
    expect(swipe(0, 0)).toBeNull()
  })
})

describe('NATIVE_BACK_EDGE_PX', () => {
  it('is narrower than the travel a swipe needs', () => {
    // The band is declined outright (ADR 0075). If it were wider than
    // `SWIPE_BACK_MIN_X`, a swipe could start outside it and still be refused
    // for reasons the reader could not see.
    expect(NATIVE_BACK_EDGE_PX).toBeLessThan(SWIPE_BACK_MIN_X)
  })
})

describe('SWIPE_BACK_MIN_X', () => {
  it('commits after an intentional drag while retaining a cancelable preview', () => {
    expect(SWIPE_BACK_MIN_X).toBeLessThan(SWIPE_MIN_X)
    expect(SWIPE_BACK_MIN_X).toBeGreaterThan(SWIPE_DECISION_THRESHOLD)
  })
})
