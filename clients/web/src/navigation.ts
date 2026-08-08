type NavigationTimingEntry = {
  type?: string
}

export type NavigationPerformance = {
  getEntriesByType(type: string): PerformanceEntryList | NavigationTimingEntry[]
}

type ThreadLoadStorage = Pick<Storage, 'getItem' | 'setItem'>

/** Returns the Navigation Timing Level 2 entry type when the engine supports it. */
export function navigationTimingType(
  performance: Pick<NavigationPerformance, 'getEntriesByType'>,
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
 * Whether this tab has already loaded a thread deep link for this room.
 *
 * Browser navigation timing does not reliably distinguish first loads from
 * reloads or history restoration across engines. A tab-scoped marker gives all
 * browsers the same contract: preserve the first deep link, then clear its
 * transient thread view on a later document load (including Back/Forward).
 */
export function shouldScrubRestoredThread(
  threadId: string,
  pathname: string = window.location.pathname,
  storage: ThreadLoadStorage = window.sessionStorage,
): boolean {
  try {
    const key = `axon.thread-document-load:${pathname}\0${threadId}`
    if (storage.getItem(key) === '1') {
      return true
    }
    storage.setItem(key, '1')
  } catch {
    // Private-mode storage failures keep the deep link rather than discarding
    // an intentional navigation without a reliable restoration signal.
  }
  return false
}
