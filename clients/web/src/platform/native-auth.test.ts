import { describe, expect, it, vi } from 'vitest'
import { memoryStorage } from '../test/memory-storage'
import { NativeSignInCancelled } from './index'
import { KeychainStorage, loadNativeAuth } from './native-auth'

type Call = (command: string, args?: Record<string, unknown>) => Promise<never>

/** A plugin double: `capabilities`, a Keychain, and a scripted Apple sheet. */
function plugin({
  ios = true,
  keychain = {} as Record<string, string>,
  apple = (): Promise<unknown> => Promise.resolve({ identityToken: 'id-tok' }),
  failWrites = false,
  failLoad = false,
} = {}) {
  const writes: string[] = []
  const call = vi.fn((command: string, args?: Record<string, unknown>) => {
    switch (command) {
      case 'capabilities':
        return Promise.resolve({ appleSignIn: ios, secureStorage: ios })
      case 'secret_load':
        if (failLoad) {
          return Promise.reject({ kind: 'failed', message: 'OSStatus -25308' })
        }
        return Promise.resolve({ entries: { ...keychain } })
      case 'secret_set':
        writes.push(`set ${String(args?.key)}`)
        if (failWrites) {
          return Promise.reject({ kind: 'failed', message: 'OSStatus -25308' })
        }
        keychain[String(args?.key)] = String(args?.value)
        return Promise.resolve(null)
      case 'secret_delete':
        writes.push(`delete ${String(args?.key)}`)
        delete keychain[String(args?.key)]
        return Promise.resolve(null)
      case 'apple_sign_in':
        return apple()
      default:
        return Promise.reject(new Error(`unexpected ${command}`))
    }
  }) as unknown as Call
  return { call, keychain, writes }
}

describe('loadNativeAuth', () => {
  it('reports nothing native off iOS', async () => {
    const { call } = plugin({ ios: false })
    const native = await loadNativeAuth(memoryStorage(), call)
    expect(native).toEqual({ secureStorage: null, appleSignIn: null })
  })

  it('falls back to the browser behaviour when the plugin cannot answer', async () => {
    const call = vi.fn(() =>
      Promise.reject(new Error('no plugin')),
    ) as unknown as Call
    const native = await loadNativeAuth(memoryStorage(), call)
    expect(native).toEqual({ secureStorage: null, appleSignIn: null })
  })

  it('serves what the Keychain holds', async () => {
    const { call } = plugin({ keychain: { 'axon.token': 'kept' } })
    const native = await loadNativeAuth(memoryStorage(), call)
    expect(native.secureStorage?.getItem('axon.token')).toBe('kept')
  })

  it('moves tokens out of localStorage into the Keychain', async () => {
    const legacy = memoryStorage({
      'axon.oauth.session': '{"accessToken":"a"}',
      'axon.other': 'untouched',
    })
    const { call, keychain } = plugin()

    const native = await loadNativeAuth(legacy, call)

    expect(keychain['axon.oauth.session']).toBe('{"accessToken":"a"}')
    expect(native.secureStorage?.getItem('axon.oauth.session')).toBe(
      '{"accessToken":"a"}',
    )
    expect(legacy.getItem('axon.oauth.session')).toBeNull()
    expect(legacy.getItem('axon.other')).toBe('untouched')
  })

  it('keeps the localStorage copy when the Keychain refuses it', async () => {
    const legacy = memoryStorage({ 'axon.token': 'only-copy' })
    const { call } = plugin({ failWrites: true })
    vi.spyOn(console, 'warn').mockImplementation(() => {})

    await loadNativeAuth(legacy, call)

    expect(legacy.getItem('axon.token')).toBe('only-copy')
  })

  it('prefers a Keychain entry over a stale localStorage one', async () => {
    const legacy = memoryStorage({ 'axon.token': 'stale' })
    const { call } = plugin({ keychain: { 'axon.token': 'current' } })

    const native = await loadNativeAuth(legacy, call)

    expect(native.secureStorage?.getItem('axon.token')).toBe('current')
    expect(legacy.getItem('axon.token')).toBeNull()
  })

  it('leaves the localStorage copy alone when the Keychain cannot be read', async () => {
    // An unreadable Keychain looks empty, so a stale legacy copy would look
    // newer than the real entry and be written over it.
    const legacy = memoryStorage({ 'axon.token': 'stale' })
    const { call, keychain, writes } = plugin({
      keychain: { 'axon.token': 'current' },
      failLoad: true,
    })
    vi.spyOn(console, 'warn').mockImplementation(() => {})

    const native = await loadNativeAuth(legacy, call)

    expect(writes).toEqual([])
    expect(keychain['axon.token']).toBe('current')
    expect(legacy.getItem('axon.token')).toBe('stale')
    expect(native.secureStorage?.getItem('axon.token')).toBeNull()
  })

  it('passes the nonce through and returns the identity token', async () => {
    const { call } = plugin()
    const native = await loadNativeAuth(memoryStorage(), call)

    await expect(native.appleSignIn?.('nonce+/=')).resolves.toBe('id-tok')
    expect(call).toHaveBeenCalledWith('apple_sign_in', { nonce: 'nonce+/=' })
  })

  it('turns a dismissed sheet into NativeSignInCancelled', async () => {
    const { call } = plugin({
      apple: () =>
        Promise.reject({ kind: 'cancelled', message: 'user cancelled' }),
    })
    const native = await loadNativeAuth(memoryStorage(), call)

    await expect(native.appleSignIn?.('n')).rejects.toBeInstanceOf(
      NativeSignInCancelled,
    )
  })

  it('reports other Apple failures as errors with their message', async () => {
    const { call } = plugin({
      apple: () => Promise.reject({ kind: 'failed', message: 'error 1000' }),
    })
    const native = await loadNativeAuth(memoryStorage(), call)

    await expect(native.appleSignIn?.('n')).rejects.toThrow('error 1000')
  })
})

describe('KeychainStorage', () => {
  it('lands writes in the order they were made', async () => {
    const { call, keychain, writes } = plugin()
    const storage = new KeychainStorage(new Map(), call)

    storage.setItem('axon.oauth.session', 'refreshed')
    storage.removeItem('axon.oauth.session')
    await storage.settled()

    expect(writes).toEqual([
      'set axon.oauth.session',
      'delete axon.oauth.session',
    ])
    expect(keychain).toEqual({})
    expect(storage.getItem('axon.oauth.session')).toBeNull()
  })

  it('reports a failed write once, and keeps writing after it', async () => {
    const { call } = plugin({ failWrites: true })
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    const storage = new KeychainStorage(new Map(), call)

    storage.setItem('a', '1')
    storage.removeItem('a')
    await expect(storage.settled()).rejects.toThrow('a Keychain write failed')
    await expect(storage.settled()).resolves.toBeUndefined()
  })
})
