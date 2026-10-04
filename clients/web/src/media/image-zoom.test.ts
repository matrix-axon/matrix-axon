import { describe, expect, it } from 'vitest'
import {
  clampPan,
  FIT,
  MAX_SCALE,
  panBy,
  zoomAt,
  zoomTransform,
} from './image-zoom'

const size = { width: 400, height: 300 }

describe('image zoom', () => {
  it('keeps the point under the anchor fixed while zooming', () => {
    // 100px right of centre, at 1x, is image point 100. Zoomed to 2x about
    // that anchor, image point 100 must still be under it: 2 * 100 + x = 100.
    const zoomed = zoomAt(FIT, 2, { x: 100, y: 0 }, size)
    expect(zoomed).toEqual({ scale: 2, x: -100, y: 0 })
    // And from a zoomed, panned state too.
    const again = zoomAt(zoomed, 3, { x: 50, y: 20 }, size)
    const before = (50 - zoomed.x) / zoomed.scale
    const after = (50 - again.x) / again.scale
    expect(after).toBeCloseTo(before)
  })

  it('clamps the scale, and returns to fit at the bottom', () => {
    expect(zoomAt(FIT, 100, { x: 0, y: 0 }, size).scale).toBe(MAX_SCALE)
    const zoomed = zoomAt(FIT, 3, { x: 120, y: 80 }, size)
    expect(zoomAt(zoomed, 0.5, { x: 0, y: 0 }, size)).toEqual(FIT)
  })

  it('cannot pan the image away, and cannot pan at all at fit', () => {
    expect(panBy(FIT, { x: 50, y: 50 }, size)).toEqual(FIT)
    const zoomed = { scale: 2, x: 0, y: 0 }
    // At 2x a 400px-wide image may move 200px either way.
    expect(panBy(zoomed, { x: 1000, y: -1000 }, size)).toEqual({
      scale: 2,
      x: 200,
      y: -150,
    })
    expect(clampPan({ scale: 1, x: 30, y: 30 }, size)).toEqual({
      scale: 1,
      x: 0,
      y: 0,
    })
  })

  it('applies no transform at fit', () => {
    expect(zoomTransform(FIT)).toBeUndefined()
    expect(zoomTransform({ scale: 2, x: 10, y: -5 })).toBe(
      'translate(10px, -5px) scale(2)',
    )
  })
})
