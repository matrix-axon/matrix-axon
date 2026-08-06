type LegacyNavigation = {
  type: number
  TYPE_RELOAD: number
  TYPE_BACK_FORWARD: number
}

type NavigationPerformance = {
  getEntriesByType(type: string): Array<{ type?: string }>
  navigation?: LegacyNavigation
}

function isFirefox(userAgent: string): boolean {
  return /(?:Firefox|FxiOS)\//.test(userAgent)
}

function isLegacyReloadOrRestore(performance: NavigationPerformance): boolean {
  const legacy = performance.navigation
  return (
    legacy !== undefined &&
    (legacy.type === legacy.TYPE_RELOAD ||
      legacy.type === legacy.TYPE_BACK_FORWARD)
  )
}

/**
 * Whether this document was reloaded or restored from browser history.
 *
 * Firefox reports a scripted reload as `navigate` in Navigation Timing Level 2
 * while retaining the correct value in its legacy navigation entry. Other
 * engines keep the modern entry authoritative, so a stale legacy value cannot
 * discard an intentional thread deep link.
 */
export function isReloadOrRestoreNavigation(
  performance: NavigationPerformance = window.performance as unknown as NavigationPerformance,
  userAgent: string = navigator.userAgent,
): boolean {
  try {
    const [entry] = performance.getEntriesByType('navigation')
    if (entry !== undefined) {
      if (entry.type === 'reload' || entry.type === 'back_forward') {
        return true
      }
      return isFirefox(userAgent) && isLegacyReloadOrRestore(performance)
    }
  } catch {
    // Some webviews have no usable Performance Timeline. The legacy entry is
    // the best available signal and keeps initial render from throwing.
  }
  return isLegacyReloadOrRestore(performance)
}
