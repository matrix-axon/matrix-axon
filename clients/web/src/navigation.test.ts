import { describe, expect, it } from 'vitest'
import { navigationTimingType, shouldScrubRestoredThread } from './navigation'

function navigationPerformance(type?: PerformanceNavigationTiming['type']) {
  return {
    getEntriesByType: () => (type === undefined ? [] : [{ type }]),
  }
}

function memoryStorage() {
  const entries = new Map<string, string>()
  return {
    getItem: (key: string) => entries.get(key) ?? null,
    setItem: (key: string, value: string) => entries.set(key, value),
  }
}

describe('navigationTimingType', () => {
  it('reads a Navigation Timing Level 2 result', () => {
    expect(navigationTimingType(navigationPerformance('reload'))).toBe('reload')
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

describe('shouldScrubRestoredThread', () => {
  it('preserves the first deep link in a tab', () => {
    expect(
      shouldScrubRestoredThread(
        '$root',
        '/account/rooms/room',
        memoryStorage(),
      ),
    ).toBe(false)
  })

  it('scrubs a reload or history restoration of the same deep link', () => {
    const storage = memoryStorage()
    expect(
      shouldScrubRestoredThread('$root', '/account/rooms/room', storage),
    ).toBe(false)
    expect(
      shouldScrubRestoredThread('$root', '/account/rooms/room', storage),
    ).toBe(true)
  })

  it('preserves a deep link when session storage is unavailable', () => {
    expect(
      shouldScrubRestoredThread('$root', '/account/rooms/room', {
        getItem: () => {
          throw new Error('Storage unavailable')
        },
        setItem: () => undefined,
      }),
    ).toBe(false)
  })
})
