import { describe, expect, it } from 'vitest'
import { isReloadOrRestoreNavigation, navigationTimingType } from './navigation'

function navigationPerformance(type?: PerformanceNavigationTiming['type']) {
  return {
    getEntriesByType: () => (type === undefined ? [] : [{ type }]),
  }
}

describe('navigationTimingType', () => {
  it('reads a Navigation Timing Level 2 result', () => {
    expect(navigationTimingType(navigationPerformance('reload'))).toBe('reload')
  })

  it('returns undefined when Navigation Timing has no entry', () => {
    expect(navigationTimingType(navigationPerformance())).toBeUndefined()
  })

  it('degrades when Navigation Timing is unavailable', () => {
    expect(
      navigationTimingType({
        getEntriesByType: () => {
          throw new Error('Navigation Timing unavailable')
        },
      }),
    ).toBeUndefined()
  })
})

describe('isReloadOrRestoreNavigation', () => {
  it.each([
    ['navigate', false],
    ['reload', true],
    ['back_forward', true],
    [undefined, false],
  ] as const)('classifies %s navigation as %s', (type, expected) => {
    expect(isReloadOrRestoreNavigation(type)).toBe(expected)
  })
})
