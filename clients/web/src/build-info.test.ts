import { describe, expect, it } from 'vitest'
import { formatVersion } from './build-info'

describe('formatVersion', () => {
  it('appends the build id to the release', () => {
    expect(formatVersion('0.1.0', 'ab12cd34ef56')).toBe('0.1.0+ab12cd34ef56')
  })

  it('keeps a real hash that happens to be dirty', () => {
    expect(formatVersion('0.1.0', 'ab12cd34ef56-dirty')).toBe(
      '0.1.0+ab12cd34ef56-dirty',
    )
  })

  it('keeps an explicit VITE_AXON_WEB_VERSION label', () => {
    expect(formatVersion('0.1.0', 'iphone-swipe-test-4')).toBe(
      '0.1.0+iphone-swipe-test-4',
    )
  })

  // A build id that identifies nothing earns no room in the footer. `-dirty` is
  // appended to whatever the hash lookup produced, so a gitless build in a
  // dirty tree reports `unknown-dirty` — which was rendering as a real id.
  it.each([
    ['no hash at all', ''],
    ['the unknown placeholder', 'unknown'],
    ['unknown in a dirty tree', 'unknown-dirty'],
  ])('degrades to the bare release for %s', (_label, version) => {
    expect(formatVersion('0.1.0', version)).toBe('0.1.0')
  })
})
