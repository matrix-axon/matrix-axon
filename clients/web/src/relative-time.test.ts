import { expect, it } from 'vitest'
import { relativeTime } from './relative-time'

const NOW = Date.UTC(2026, 10, 11, 12, 0, 0)

it.each([
  [30_000, 'now'],
  [10 * 60_000, '10m'],
  [59 * 60_000, '59m'],
  [3 * 3_600_000, '3h'],
  [26 * 3_600_000, '1d'],
  [6 * 86_400_000, '6d'],
  [8 * 86_400_000, '2026-11-03'],
])('%i ms ago reads %s', (ago, shown) => {
  expect(relativeTime(NOW - ago, NOW)).toBe(shown)
})
