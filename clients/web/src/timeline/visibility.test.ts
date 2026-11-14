import { describe, expect, it } from 'vitest'
import { hiddenByRedaction } from './visibility'

const redacted = { event_id: '$root', redacted: true }
const live = { event_id: '$live', redacted: false }
const roots = (count: number) => new Map([['$root', { reply_count: count }]])

describe('hiddenByRedaction', () => {
  it('hides a redacted event only when the setting is on', () => {
    expect(hiddenByRedaction(redacted, true)).toBe(true)
    expect(hiddenByRedaction(redacted, false)).toBe(false)
    expect(hiddenByRedaction(live, true)).toBe(false)
  })

  it('never hides a redacted thread root that still has replies', () => {
    expect(hiddenByRedaction(redacted, true, roots(2))).toBe(false)
  })

  it('hides a redacted root whose thread has no surviving replies', () => {
    expect(hiddenByRedaction(redacted, true, roots(0))).toBe(true)
    expect(hiddenByRedaction(redacted, true, new Map())).toBe(true)
  })
})
