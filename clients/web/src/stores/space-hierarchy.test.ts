import { expect, it } from 'vitest'
import {
  hierarchyChild,
  hierarchyPage,
  spaceDiscovery,
} from '../test/room-details'
import {
  appendPage,
  crawlPaused,
  crawlRetryable,
  HIERARCHY_MAX_PAGE,
  listFromPage,
  mergeReread,
  rereadLimit,
  type HierarchyList,
} from './space-hierarchy'

const rows = (count: number, prefix = 'r', from = 0) =>
  Array.from({ length: count }, (_, i) =>
    hierarchyChild(`!${prefix}-${from + i}:hs`),
  )
const ids = (list: HierarchyList) => list.children.map((row) => row.room_id)

it('keeps each room once within a page and across pages', () => {
  const first = listFromPage(
    hierarchyPage([...rows(2), hierarchyChild('!r-0:hs')], {
      next_cursor: 'c1',
      total_children: 3,
    }),
  )
  expect(ids(first)).toEqual(['!r-0:hs', '!r-1:hs'])
  const next = appendPage(first, hierarchyPage(rows(2, 'r', 1)))
  expect(ids(next)).toEqual(['!r-0:hs', '!r-1:hs', '!r-2:hs'])
  expect(next.nextCursor).toBeNull()
})

it('re-reads at least a page and at most the server maximum', () => {
  expect(rereadLimit(null)).toBe(50)
  expect(rereadLimit(listFromPage(hierarchyPage(rows(3))))).toBe(50)
  expect(rereadLimit(listFromPage(hierarchyPage(rows(120))))).toBe(120)
  expect(rereadLimit(listFromPage(hierarchyPage(rows(900))))).toBe(
    HIERARCHY_MAX_PAGE,
  )
})

it('a re-read of 500 rows or fewer replaces the list and its cursor', () => {
  const list = listFromPage(hierarchyPage(rows(80), { next_cursor: 'old' }))
  const merged = mergeReread(
    list,
    hierarchyPage(rows(79), { next_cursor: 'new' }),
    'frame',
  )
  expect(merged.children).toHaveLength(79)
  expect(merged.nextCursor).toBe('new')
})

const loaded650 = () =>
  listFromPage(hierarchyPage(rows(650), { next_cursor: 'tail' }))

it('a frame re-read past 500 rows keeps the tail and the tail cursor', () => {
  // The first 500 loaded come back (a crawl changed summaries, not links).
  const same = mergeReread(
    loaded650(),
    hierarchyPage(rows(500), { next_cursor: 'head' }),
    'frame',
  )
  expect(same.children).toHaveLength(650)
  expect(same.nextCursor).toBe('tail')

  // A tail row the refreshed rows also contain is shown once, where they put
  // it. (At limit 500 a response holding all 500 loaded head rows has no room
  // for one, so this is the ADR's guarantee rather than a case the server
  // produces today.)
  const moved = mergeReread(
    loaded650(),
    hierarchyPage([...rows(500), hierarchyChild('!r-600:hs')], {
      next_cursor: 'head',
    }),
    'frame',
  )
  expect(moved.children).toHaveLength(650)
  expect(ids(moved).filter((id) => id === '!r-600:hs')).toHaveLength(1)
  expect(ids(moved)[500]).toBe('!r-600:hs')
  expect(moved.nextCursor).toBe('tail')
})

it('a frame re-read drops the tail when a loaded head row is missing', () => {
  // A child linked at the head pushes `!r-499` out of the 500-row response;
  // the kept tail would start after it, so it would vanish.
  const merged = mergeReread(
    loaded650(),
    hierarchyPage([hierarchyChild('!new:hs'), ...rows(499)], {
      next_cursor: 'head',
    }),
    'frame',
  )
  expect(merged.children).toHaveLength(500)
  expect(merged.nextCursor).toBe('head')
})

it('a reconnect re-read past 500 rows always drops the tail', () => {
  const merged = mergeReread(
    loaded650(),
    hierarchyPage(rows(500), { next_cursor: 'head' }),
    'reconnect',
  )
  expect(merged.children).toHaveLength(500)
  expect(merged.nextCursor).toBe('head')
})

it('takes the crawl status and total from the newest page', () => {
  const merged = mergeReread(
    loaded650(),
    hierarchyPage(rows(500), {
      total_children: 700,
      discovery: spaceDiscovery({ state: 'running', rooms_cached: 9 }),
    }),
    'frame',
  )
  expect(merged.totalChildren).toBe(700)
  expect(merged.discovery.rooms_cached).toBe(9)
})

it('classifies a crawl as retryable (row 5) or paused (row 6) in table order', () => {
  const failed = (extra: object) =>
    spaceDiscovery({ outcome: 'failed', ...extra })
  expect(crawlRetryable(failed({ failure: 'timeout' }))).toBe(true)
  expect(crawlRetryable(failed({ failure: null }))).toBe(true)
  expect(crawlRetryable(failed({ failure: 'forbidden' }))).toBe(false)
  expect(crawlRetryable(failed({ failure: 'not_found' }))).toBe(false)
  expect(
    crawlRetryable(failed({ failure: 'timeout', support: 'unsupported' })),
  ).toBe(false)
  expect(crawlRetryable(failed({ failure: 'timeout', state: 'queued' }))).toBe(
    false,
  )
  expect(crawlPaused(spaceDiscovery({ outcome: 'in_progress' }))).toBe(true)
  expect(
    crawlPaused(spaceDiscovery({ outcome: 'in_progress', state: 'running' })),
  ).toBe(false)
  expect(
    crawlPaused(
      spaceDiscovery({ outcome: 'in_progress', support: 'unsupported' }),
    ),
  ).toBe(false)
})
