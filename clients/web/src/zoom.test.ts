import { describe, expect, it } from 'vitest'
import { parseZoom, stepZoom, ZOOM_LEVELS } from './zoom'

describe('stepZoom (ADR 0107)', () => {
  it('walks the browser zoom levels and clamps at the ends', () => {
    expect(stepZoom(1, 1)).toBe(1.1)
    expect(stepZoom(1, -1)).toBe(0.9)
    expect(stepZoom(ZOOM_LEVELS.at(-1)!, 1)).toBe(ZOOM_LEVELS.at(-1))
    expect(stepZoom(ZOOM_LEVELS[0], -1)).toBe(ZOOM_LEVELS[0])
  })

  it('steps from a value between levels to its neighbour', () => {
    expect(stepZoom(1.2, 1)).toBe(1.25)
    expect(stepZoom(1.2, -1)).toBe(1.1)
  })
})

describe('parseZoom', () => {
  it('keeps a usable factor and defaults anything else', () => {
    expect(parseZoom(1.5)).toBe(1.5)
    expect(parseZoom(undefined)).toBe(1)
    expect(parseZoom(Infinity)).toBe(1)
    expect(parseZoom(0.2)).toBe(1)
  })
})
