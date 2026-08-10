import { describe, expect, it } from 'vitest'
import {
  isReloadOrRestoreNavigation,
  legacyReloadOrRestore,
  navigationTimingType,
  type NavigationPerformance,
  type NavigationSignals,
} from './navigation'

/** The real Level 1 constants, so a stub cannot accidentally agree by luck. */
const TYPE_NAVIGATE = 0
const TYPE_RELOAD = 1
const TYPE_BACK_FORWARD = 2
const LEGACY_CONSTANTS = {
  TYPE_RELOAD,
  TYPE_BACK_FORWARD,
}

/** An engine with a Level 2 entry (or none, when `type` is omitted). */
function navigationPerformance(
  type?: PerformanceNavigationTiming['type'],
): NavigationPerformance {
  return {
    getEntriesByType: () => (type === undefined ? [] : [{ type }]),
  }
}

/** An engine with no Level 2 entry, only the deprecated Level 1 reading. */
function legacyPerformance(
  navigation: Record<string, unknown> | undefined,
): NavigationPerformance {
  return { getEntriesByType: () => [], navigation }
}

/** A hardened engine: reading either API throws rather than returning nothing. */
const hostilePerformance: NavigationPerformance = {
  getEntriesByType: () => {
    throw new Error('Performance Timeline is restricted')
  },
  get navigation(): never {
    throw new Error('Performance Timeline is restricted')
  },
}

function signals(
  overrides: Partial<NavigationSignals> = {},
): NavigationSignals {
  return {
    timingType: undefined,
    legacyReloadOrRestore: false,
    scriptedReload: false,
    ...overrides,
  }
}

describe('navigationTimingType', () => {
  it('reads a Navigation Timing Level 2 result', () => {
    expect(navigationTimingType(navigationPerformance('reload'))).toBe('reload')
  })

  it('returns undefined when Navigation Timing has no entry', () => {
    expect(navigationTimingType(navigationPerformance())).toBeUndefined()
  })

  it('degrades when Navigation Timing throws', () => {
    expect(navigationTimingType(hostilePerformance)).toBeUndefined()
  })

  it('degrades when there is no performance object at all', () => {
    expect(navigationTimingType(undefined)).toBeUndefined()
  })
})

describe('legacyReloadOrRestore', () => {
  it.each([
    ['a reload', TYPE_RELOAD, true],
    ['a history restore', TYPE_BACK_FORWARD, true],
    ['an ordinary navigation', TYPE_NAVIGATE, false],
  ] as const)('reads %s', (_label, type, expected) => {
    expect(
      legacyReloadOrRestore(legacyPerformance({ ...LEGACY_CONSTANTS, type })),
    ).toBe(expected)
  })

  it('reads an absent legacy API as "not a reload"', () => {
    expect(legacyReloadOrRestore(legacyPerformance(undefined))).toBe(false)
  })

  // A shim that keeps `navigation` but drops the constants would otherwise
  // compare `undefined === undefined` and report every load as a reload.
  it('reads a partial legacy shim as "not a reload"', () => {
    expect(legacyReloadOrRestore(legacyPerformance({}))).toBe(false)
    expect(
      legacyReloadOrRestore(legacyPerformance({ type: TYPE_RELOAD })),
    ).toBe(false)
  })

  it('degrades when the legacy accessor itself throws', () => {
    expect(legacyReloadOrRestore(hostilePerformance)).toBe(false)
  })

  it('degrades when there is no performance object at all', () => {
    expect(legacyReloadOrRestore(undefined)).toBe(false)
  })
})

describe('isReloadOrRestoreNavigation', () => {
  // Every supported signal state, per the decision table on the function.
  it.each([
    ['L2 navigate', signals({ timingType: 'navigate' }), false],
    ['L2 reload', signals({ timingType: 'reload' }), true],
    ['L2 back_forward', signals({ timingType: 'back_forward' }), true],
    ['L2 prerender', signals({ timingType: 'prerender' }), false],
    ['no signal at all', signals(), false],
    // The modern answer wins: a genuine deep link must survive an engine whose
    // deprecated Level 1 reading disagrees.
    [
      'L2 navigate over a stale L1 reload',
      signals({ timingType: 'navigate', legacyReloadOrRestore: true }),
      false,
    ],
    // ...but with no modern answer, Level 1 is all there is. This is the
    // behaviour every engine had before this stack.
    ['L1 reload alone', signals({ legacyReloadOrRestore: true }), true],
    // Axon's own reload, which Firefox reports as `navigate`.
    [
      'a scripted reload Firefox calls a navigation',
      signals({ timingType: 'navigate', scriptedReload: true }),
      true,
    ],
    [
      'a scripted reload with no timing entry',
      signals({ scriptedReload: true }),
      true,
    ],
  ] as const)('classifies %s', (_label, given, expected) => {
    expect(isReloadOrRestoreNavigation(given)).toBe(expected)
  })
})

// The classifier is pure, so these wire the real readers to it the way boot
// does — the combination is where the regressions in this stack actually lived.
describe('a browser without Navigation Timing Level 2', () => {
  function classify(performance: NavigationPerformance | undefined): boolean {
    return isReloadOrRestoreNavigation({
      timingType: navigationTimingType(performance),
      legacyReloadOrRestore: legacyReloadOrRestore(performance),
      scriptedReload: false,
    })
  }

  it('still scrubs on a reload it can only see through the legacy API', () => {
    expect(
      classify(legacyPerformance({ ...LEGACY_CONSTANTS, type: TYPE_RELOAD })),
    ).toBe(true)
  })

  it('keeps a deep link on a genuine navigation', () => {
    expect(
      classify(legacyPerformance({ ...LEGACY_CONSTANTS, type: TYPE_NAVIGATE })),
    ).toBe(false)
  })

  it('keeps a deep link when both APIs are unreadable', () => {
    expect(classify(hostilePerformance)).toBe(false)
    expect(classify(undefined)).toBe(false)
  })
})
