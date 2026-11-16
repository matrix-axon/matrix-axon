import { describe, expect, it } from 'vitest'
import { hiddenByRedaction, NO_THREAD_ROOTS } from './visibility'

const redacted = { event_id: '$root', redacted: true }
const live = { event_id: '$live', redacted: false }

describe('hiddenByRedaction', () => {
  it('hides a redacted event only when the setting is on', () => {
    expect(hiddenByRedaction(redacted, true, NO_THREAD_ROOTS)).toBe(true)
    expect(hiddenByRedaction(redacted, false, NO_THREAD_ROOTS)).toBe(false)
    expect(hiddenByRedaction(live, true, NO_THREAD_ROOTS)).toBe(false)
  })

  it('never hides a redacted thread root that still has replies', () => {
    expect(hiddenByRedaction(redacted, true, new Set(['$root']))).toBe(false)
  })

  it('hides a redacted root that is not in the live set', () => {
    expect(hiddenByRedaction(redacted, true, new Set(['$other']))).toBe(true)
  })
})
