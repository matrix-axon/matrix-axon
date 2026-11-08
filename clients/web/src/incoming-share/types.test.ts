import { expect, it } from 'vitest'
import fixture from './fixtures/manifest-v1.json'
import { canonicalShare, shareCompleted, shareOperations } from './types'
import type { IncomingShare } from './types'

it.each([1, 2, 3] as const)(
  'upgrades legacy version %i without resending the delivered source',
  (version) => {
    const legacy = structuredClone(fixture) as IncomingShare
    legacy.version = version
    const current = canonicalShare(legacy)
    expect(current.version).toBe(4)
    expect(current.text).toBe(legacy.text)
    expect(current.previews).toEqual(legacy.previews)
    expect(current.operation.state).toBe('sent')
    expect(current.operation.eventId).toBe('$confirmed')
    expect(
      current.files
        .filter((file) => !file.thumbnail)
        .map((file) => file.caption),
    ).toEqual(['', ''])
    expect(shareCompleted(current)).toBe(false)
  },
)
it('keeps legacy uncertain source delivery uncertain and does not replay it', () => {
  const legacy = structuredClone(fixture) as IncomingShare
  legacy.files[1].operation!.state = 'unknown'
  delete legacy.files[1].operation!.eventId
  const current = canonicalShare(legacy)
  expect(current.operation.state).toBe('unknown')
  expect(shareOperations(current).map((op) => op.state)).toEqual([
    'unknown',
    'unknown',
    'unknown',
  ])
})
it('stores every modern caption uniformly, independently of source text', () => {
  const current = canonicalShare(structuredClone(fixture) as IncomingShare)
  current.text = ''
  current.files[1].caption = 'First caption'
  current.files[2].caption = 'Second caption'
  expect(canonicalShare(current)).toEqual(current)
  expect(shareOperations(current)).toHaveLength(2)
  current.text = 'Independent message'
  expect(shareOperations(current)).toHaveLength(3)
})

it.each([1, 2, 3] as const)(
  'keeps whitespace-only v%i source operations independent',
  (version) => {
    for (const state of ['sent', 'dispatching', 'unknown'] as const) {
      const legacy = structuredClone(fixture) as IncomingShare
      legacy.version = version
      legacy.text = ' \t\n'
      legacy.files[1].operation!.state = state
      const operation = structuredClone(legacy.operation)
      const current = canonicalShare(legacy)
      expect(current.operation).toEqual(operation)
      expect(shareOperations(current)).toHaveLength(2)
      expect(current.files[1].operation!.state).toBe(state)
    }
  },
)
