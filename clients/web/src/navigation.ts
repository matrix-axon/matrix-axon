type NavigationTimingEntry = {
  type?: string
}

export type NavigationPerformance = {
  getEntriesByType(type: string): PerformanceEntryList | NavigationTimingEntry[]
}

/** Returns the Navigation Timing Level 2 entry type when the engine supports it. */
export function navigationTimingType(
  performance: NavigationPerformance,
): string | undefined {
  try {
    return (
      performance.getEntriesByType('navigation')[0] as
        NavigationTimingEntry | undefined
    )?.type
  } catch {
    return undefined
  }
}

/**
 * The document's navigation classification, sampled once while it is stable at
 * boot. Startup routing and performance telemetry intentionally share this
 * value so their view of the document cannot drift.
 */
export const bootNavigationTimingType = navigationTimingType(performance)

/**
 * Whether standard navigation timing identifies a reload or history restore.
 * Axon's automatic reload is tracked separately in reload.ts because Firefox
 * can report that scripted reload as `navigate`.
 */
export function isReloadOrRestoreNavigation(
  navigationType: string | undefined = bootNavigationTimingType,
): boolean {
  return navigationType === 'reload' || navigationType === 'back_forward'
}
