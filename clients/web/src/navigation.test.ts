import { describe, expect, it } from 'vitest'
import { isReloadOrRestoreNavigation } from './navigation'

function navigationPerformance(
  modern: PerformanceNavigationTiming['type'] | undefined,
  legacyType: number,
  options: { throws?: boolean; legacyThrows?: boolean } = {},
) {
  return {
    getEntriesByType: () => {
      if (options.throws) {
        throw new Error('Navigation Timing unavailable')
      }
      return modern === undefined ? [] : [{ type: modern }]
    },
    get navigation() {
      if (options.legacyThrows) {
        throw new Error('Legacy Navigation Timing unavailable')
      }
      return { type: legacyType, TYPE_RELOAD: 1, TYPE_BACK_FORWARD: 2 }
    },
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

  it('preserves a Firefox deep link when both navigation signals identify a genuine navigation', () => {
    expect(
      isReloadOrRestoreNavigation(
        navigationPerformance('navigate', 0),
        'Mozilla/5.0 Firefox/149.0',
      ),
    ).toBe(false)
  })

  it('does not trust a legacy value outside Firefox when Navigation Timing is unavailable', () => {
    expect(
      isReloadOrRestoreNavigation(
        navigationPerformance(undefined, 2, { throws: true }),
        'Chrome',
      ),
    ).toBe(false)
  })

  it('degrades gracefully when both navigation APIs are unavailable', () => {
    expect(
      isReloadOrRestoreNavigation(
        navigationPerformance(undefined, 2, {
          throws: true,
          legacyThrows: true,
        }),
        'Mozilla/5.0 Firefox/149.0',
      ),
    ).toBe(false)
  })
})
