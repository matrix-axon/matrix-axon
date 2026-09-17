import { afterEach, describe, expect, it, vi } from 'vitest'
import { randomId } from './random-id'

const UUID_V4 =
  /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/

/**
 * Replace `globalThis.crypto` for one test. It is a getter on the global in
 * some environments, so it is redefined rather than assigned.
 */
function withCrypto(replacement: unknown): void {
  const original = Object.getOwnPropertyDescriptor(globalThis, 'crypto')
  Object.defineProperty(globalThis, 'crypto', {
    value: replacement,
    configurable: true,
    writable: true,
  })
  restore = () => {
    if (original === undefined) {
      delete (globalThis as { crypto?: unknown }).crypto
    } else {
      Object.defineProperty(globalThis, 'crypto', original)
    }
  }
}

let restore: (() => void) | null = null
afterEach(() => {
  restore?.()
  restore = null
})

describe('randomId', () => {
  it('uses crypto.randomUUID when the origin is secure', () => {
    const randomUUID = vi.fn(() => '11111111-2222-4333-8444-555555555555')
    withCrypto({ randomUUID, getRandomValues: () => {} })

    expect(randomId()).toBe('11111111-2222-4333-8444-555555555555')
    expect(randomUUID).toHaveBeenCalledTimes(1)
  })

  /**
   * The insecure-origin case, which is a plain-http LAN origin. `randomUUID`
   * is secure-context only and simply absent there; `getRandomValues` is not
   * gated, so a real v4 UUID is still available from its bytes.
   */
  it('builds a v4 UUID from getRandomValues when randomUUID is absent', () => {
    const getRandomValues = vi.fn((bytes: Uint8Array) => {
      bytes.fill(0xff)
      return bytes
    })
    withCrypto({ getRandomValues })

    const id = randomId()

    expect(getRandomValues).toHaveBeenCalledTimes(1)
    expect(id).toMatch(UUID_V4)
    // The version and variant nibbles are forced, so all-ones bytes still
    // produce a well-formed v4 rather than `ffffffff-ffff-ffff-ffff-…`.
    expect(id).toBe('ffffffff-ffff-4fff-bfff-ffffffffffff')
  })

  it('still returns an id when there is no crypto at all', () => {
    withCrypto(undefined)

    expect(randomId()).toMatch(UUID_V4)
  })

  it('does not repeat itself', () => {
    const ids = new Set(Array.from({ length: 500 }, () => randomId()))

    expect(ids.size).toBe(500)
  })
})
