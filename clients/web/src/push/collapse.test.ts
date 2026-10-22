import { describe, expect, it } from 'vitest'
import { notificationCollapseId } from './collapse'

describe('notificationCollapseId', () => {
  it('uses the first 32 hex characters of SHA-256', async () => {
    // SHA-256("abc") is the standard vector.
    expect(await notificationCollapseId('abc')).toBe(
      'ba7816bf8f01cfea414140de5dae2223',
    )
  })

  it('changes with the room id', async () => {
    const first = await notificationCollapseId('!a:example')
    const second = await notificationCollapseId('!b:example')
    expect(first).not.toBe(second)
    expect(first).toMatch(/^[0-9a-f]{32}$/)
  })
})
