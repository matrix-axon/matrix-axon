import { invoke } from '@tauri-apps/api/core'
import { SESSION_KEY } from '../auth/oauth'
import { STORAGE_KEY as TOKEN_KEY } from '../auth/token-paste'
import { NativeSignInCancelled, type SecureStorage } from './index'

/**
 * The shell's native sign-in pieces (ADR 0054): the Keychain and the Sign in
 * with Apple sheet, both implemented by the in-repo `native-auth` plugin
 * (`src-tauri/native-auth`) and present on iOS only.
 */
export interface NativeAuth {
  secureStorage: SecureStorage | null
  appleSignIn: ((nonce: string) => Promise<string>) | null
}

export const NO_NATIVE_AUTH: NativeAuth = {
  secureStorage: null,
  appleSignIn: null,
}

/**
 * The credentials an earlier build of the shell kept in `localStorage`. Moved
 * into the Keychain on first launch of a build that has one, so upgrading does
 * not sign anyone out — and then deleted, so the plaintext copy does not
 * linger beside the protected one.
 */
const MIGRATED_KEYS = [TOKEN_KEY, SESSION_KEY] as const

interface Capabilities {
  appleSignIn: boolean
  secureStorage: boolean
}

/** The plugin's error shape (`native-auth/src/lib.rs`, `Error`). */
interface PluginError {
  kind: 'cancelled' | 'unsupported' | 'failed'
  message: string
}

function isPluginError(value: unknown): value is PluginError {
  return (
    typeof value === 'object' &&
    value !== null &&
    typeof (value as { kind?: unknown }).kind === 'string' &&
    typeof (value as { message?: unknown }).message === 'string'
  )
}

type Invoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>

const pluginInvoke: Invoke = (command, args) =>
  invoke(`plugin:native-auth|${command}`, args)

/**
 * Ask the shell what it supports and, where there is a Keychain, load it.
 *
 * Runs once, before the first render, because `AuthPersistence` reads
 * synchronously while the service graph is built. Never rejects: a shell
 * without the plugin, or one that cannot answer, gets the browser's behaviour
 * rather than no app.
 */
export async function loadNativeAuth(
  legacy: Storage = window.localStorage,
  call: Invoke = pluginInvoke,
): Promise<NativeAuth> {
  let capabilities: Capabilities
  try {
    capabilities = await call<Capabilities>('capabilities')
  } catch (error) {
    console.warn('native auth unavailable', describe(error))
    return NO_NATIVE_AUTH
  }
  return {
    secureStorage: capabilities.secureStorage
      ? await loadKeychain(legacy, call)
      : null,
    appleSignIn: capabilities.appleSignIn
      ? (nonce) => appleSignIn(nonce, call)
      : null,
  }
}

async function appleSignIn(nonce: string, call: Invoke): Promise<string> {
  let response: { identityToken?: unknown }
  try {
    response = await call('apple_sign_in', { nonce })
  } catch (error) {
    if (isPluginError(error) && error.kind === 'cancelled') {
      throw new NativeSignInCancelled(error.message)
    }
    throw new Error(describe(error), { cause: error })
  }
  if (typeof response.identityToken !== 'string') {
    throw new Error('Sign in with Apple returned no identity token')
  }
  return response.identityToken
}

async function loadKeychain(
  legacy: Storage,
  call: Invoke,
): Promise<SecureStorage> {
  let entries: Record<string, string> = {}
  try {
    ;({ entries } = await call<{ entries: Record<string, string> }>(
      'secret_load',
    ))
  } catch (error) {
    // Whatever the Keychain held is out of reach this launch, so the app
    // starts signed out. Still hand back a Keychain-backed store rather than
    // falling back to `localStorage`: a sign-in now should land where the
    // next launch will look for it, not in plaintext.
    console.warn('could not read the Keychain', describe(error))
  }
  const storage = new KeychainStorage(new Map(Object.entries(entries)), call)
  for (const key of MIGRATED_KEYS) {
    const value = readLegacy(legacy, key)
    if (value === null) {
      continue
    }
    // A Keychain entry is newer than any `localStorage` one: the shell stops
    // writing the latter as soon as it has the former.
    if (storage.getItem(key) === null) {
      storage.setItem(key, value)
    }
    // Deleted only once the Keychain has it; a failed write keeps the one
    // copy there is.
    const landed = await storage.settled().then(
      () => true,
      () => false,
    )
    if (landed) {
      try {
        legacy.removeItem(key)
      } catch {
        // Nothing more to do; the next launch tries again.
      }
    }
  }
  return storage
}

function readLegacy(storage: Storage, key: string): string | null {
  try {
    return storage.getItem(key)
  } catch {
    return null
  }
}

/**
 * The in-memory copy plus an ordered write-behind queue to the Keychain.
 *
 * Ordered because two writes to one key must land in the order they were
 * made: a token refresh followed by a sign-out must not reach the Keychain as
 * a delete followed by the refreshed token.
 */
export class KeychainStorage implements SecureStorage {
  private tail: Promise<void> = Promise.resolve()
  private failed = false
  private readonly values: Map<string, string>
  private readonly call: Invoke

  constructor(values: Map<string, string>, call: Invoke) {
    this.values = values
    this.call = call
  }

  get length(): number {
    return this.values.size
  }

  key(index: number): string | null {
    return [...this.values.keys()][index] ?? null
  }

  getItem(key: string): string | null {
    return this.values.get(key) ?? null
  }

  setItem(key: string, value: string): void {
    this.values.set(key, String(value))
    this.enqueue(() => this.call('secret_set', { key, value: String(value) }))
  }

  removeItem(key: string): void {
    this.values.delete(key)
    this.enqueue(() => this.call('secret_delete', { key }))
  }

  clear(): void {
    for (const key of [...this.values.keys()]) {
      this.removeItem(key)
    }
  }

  /**
   * Resolves once every write made so far has reached the Keychain, and
   * rejects if any write since the previous `settled` failed.
   */
  settled(): Promise<void> {
    return this.tail.then(() => {
      if (this.failed) {
        this.failed = false
        throw new Error('a Keychain write failed')
      }
    })
  }

  private enqueue(write: () => Promise<unknown>): void {
    this.tail = this.tail.then(write).then(
      () => undefined,
      (error: unknown) => {
        // Logged without the key's value, and never rethrown into the chain:
        // one failed write must not stop the ones queued behind it.
        this.failed = true
        console.warn('Keychain write failed', describe(error))
      },
    )
  }
}

function describe(error: unknown): string {
  if (isPluginError(error)) {
    return error.message
  }
  if (error instanceof Error) {
    return error.message
  }
  return typeof error === 'string' ? error : 'unknown error'
}
