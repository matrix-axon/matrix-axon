import { describe, expect, it } from 'vitest'
import { isReloadOrRestoreNavigation } from './navigation'

function navigationPerformance(
  modern: PerformanceNavigationTiming['type'] | undefined,
  legacyType: number,
  throws = false,
) {
  return {
    getEntriesByType: () => {
      if (throws) {
        throw new Error('Navigation Timing unavailable')
      }
      return modern === undefined ? [] : [{ type: modern }]
    },
    navigation: { type: legacyType, TYPE_RELOAD: 1, TYPE_BACK_FORWARD: 2 },
  }
}

describe('isReloadOrRestoreNavigation', () => {
  it('uses the modern result for non-Firefox navigations', () => {
    expect(
      isReloadOrRestoreNavigation(
        navigationPerformance('navigate', 1),
        'Chrome',
      ),
    ).toBe(false)
  })

  it('uses Firefox legacy navigation for a scripted reload reported as navigate', () => {
    expect(
      isReloadOrRestoreNavigation(
        navigationPerformance('navigate', 1),
        'Mozilla/5.0 Firefox/149.0',
      ),
    ).toBe(true)
  })

  it('uses the legacy entry when Navigation Timing is unavailable', () => {
    expect(
      isReloadOrRestoreNavigation(
        navigationPerformance(undefined, 2, true),
        'Chrome',
      ),
    ).toBe(true)
  })
})
