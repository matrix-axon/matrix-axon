type LegacyNavigation = {
  type: number
  TYPE_RELOAD: number
  TYPE_BACK_FORWARD: number
}

export type NavigationPerformance = {
  getEntriesByType(type: string): Array<{ type?: string }>
  navigation?: LegacyNavigation
}

function isFirefox(userAgent: string): boolean {
  return /Firefox\//.test(userAgent)
}

function isLegacyReloadOrRestore(performance: NavigationPerformance): boolean {
  try {
    const legacy = performance.navigation
    return (
      legacy !== undefined &&
      (legacy.type === legacy.TYPE_RELOAD ||
        legacy.type === legacy.TYPE_BACK_FORWARD)
    )
  } catch {
    return false
  }
}

/** Returns the Navigation Timing Level 2 entry type when the engine supports it. */
export function navigationTimingType(
  performance: Pick<NavigationPerformance, 'getEntriesByType'>,
): string | undefined {
  try {
    return performance.getEntriesByType('navigation')[0]?.type
  } catch {
    return undefined
  }
}

/**
 * Whether this document was reloaded or restored from browser history.
 *
 * Firefox reports the scripted reload exercised by the browser regression test
 * as `navigate` in Navigation Timing Level 2. Its legacy navigation entry is a
 * compatibility signal for that Firefox-only discrepancy; the modern entry
 * remains authoritative in every other engine.
 */
export function isReloadOrRestoreNavigation(
  performance: NavigationPerformance = window.performance as unknown as NavigationPerformance,
  userAgent: string = navigator.userAgent,
): boolean {
  const navigationType = navigationTimingType(performance)
  if (navigationType === 'reload' || navigationType === 'back_forward') {
    return true
  }

  // A missing/throwing modern API must not broaden the legacy fallback to
  // engines whose stale value could erase an intentional thread deep link.
  return isFirefox(userAgent) && isLegacyReloadOrRestore(performance)
}
