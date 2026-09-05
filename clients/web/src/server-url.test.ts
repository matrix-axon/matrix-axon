import { describe, expect, it, vi } from 'vitest'
import {
  apiUrl,
  clearStoredServerUrl,
  normalizeServerUrl,
  readStoredServerUrl,
  SERVER_URL_KEY,
  storeServerUrl,
} from './server-url'
import { memoryStorage } from './test/memory-storage'

describe('normalizeServerUrl', () => {
  it('assumes https for a bare host, since this is typed by hand', () => {
    expect(normalizeServerUrl('axon.example.com')).toBe(
      'https://axon.example.com',
    )
    expect(normalizeServerUrl('  axon.example.com  ')).toBe(
      'https://axon.example.com',
    )
  })

  it('keeps an explicit scheme, including plain http', () => {
    // A LAN axon on plain http is a supported deployment — deploy/web/Caddyfile
    // says so outright — and the shell reaches it through Rust, so there is no
    // mixed-content rule to fall foul of (ADR 0102 § 2).
    expect(normalizeServerUrl('http://192.168.1.5:8080')).toBe(
      'http://192.168.1.5:8080',
    )
    expect(normalizeServerUrl('https://axon.example.com')).toBe(
      'https://axon.example.com',
    )
  })

  it('strips the trailing slash so /v1 paths concatenate cleanly', () => {
    expect(normalizeServerUrl('https://axon.example.com/')).toBe(
      'https://axon.example.com',
    )
  })

  it('preserves a path prefix, for a deployment under a subpath', () => {
    expect(normalizeServerUrl('https://example.com/axon/')).toBe(
      'https://example.com/axon',
    )
  })

  it('drops query and fragment, which would corrupt every derived path', () => {
    expect(normalizeServerUrl('https://axon.example.com/?a=1#x')).toBe(
      'https://axon.example.com',
    )
  })

  it('rejects a scheme that is not http(s)', () => {
    // This value is handed to fetch and used to build a WebSocket URL, so the
    // scheme is a security boundary, not a formatting preference.
    expect(normalizeServerUrl('javascript:alert(1)')).toBeNull()
    expect(normalizeServerUrl('data:text/html,x')).toBeNull()
    expect(normalizeServerUrl('file:///etc/passwd')).toBeNull()
    expect(normalizeServerUrl('tauri://localhost')).toBeNull()
  })

  it('rejects empty and unparseable entries', () => {
    expect(normalizeServerUrl('')).toBeNull()
    expect(normalizeServerUrl('   ')).toBeNull()
    expect(normalizeServerUrl('http://')).toBeNull()
  })

  it('treats a protocol-relative entry as a bare host', () => {
    expect(normalizeServerUrl('//axon.example.com')).toBe(
      'https://axon.example.com',
    )
  })
})

describe('stored server url', () => {
  it('round-trips through storage, normalised', () => {
    const storage = memoryStorage()
    expect(storeServerUrl(storage, 'axon.example.com/')).toBe(
      'https://axon.example.com',
    )
    expect(storage.getItem(SERVER_URL_KEY)).toBe('https://axon.example.com')
    expect(readStoredServerUrl(storage)).toBe('https://axon.example.com')
  })

  it('refuses to store an invalid entry', () => {
    const storage = memoryStorage()
    expect(storeServerUrl(storage, 'javascript:alert(1)')).toBeNull()
    expect(storage.getItem(SERVER_URL_KEY)).toBeNull()
  })

  it('re-normalises on read, so a hand-edited value cannot slip through', () => {
    const storage = memoryStorage()
    storage.setItem(SERVER_URL_KEY, 'javascript:alert(1)')
    expect(readStoredServerUrl(storage)).toBeNull()
  })

  it('clears', () => {
    const storage = memoryStorage()
    storeServerUrl(storage, 'https://axon.example.com')
    clearStoredServerUrl(storage)
    expect(readStoredServerUrl(storage)).toBeNull()
  })

  it('survives storage that throws, as private modes do', () => {
    const storage = {
      getItem: vi.fn(() => {
        throw new Error('denied')
      }),
      setItem: vi.fn(() => {
        throw new Error('denied')
      }),
      removeItem: vi.fn(() => {
        throw new Error('denied')
      }),
    } as unknown as Storage

    expect(readStoredServerUrl(storage)).toBeNull()
    // The connection still succeeds; it just will not be remembered.
    expect(storeServerUrl(storage, 'https://axon.example.com')).toBe(
      'https://axon.example.com',
    )
    expect(() => clearStoredServerUrl(storage)).not.toThrow()
  })
})

describe('a base with a path prefix', () => {
  it('survives into the websocket URL', () => {
    // `new URL('/v1/ws', base)` resolves against the origin and drops `/axon`.
    // `normalizeServerUrl` promises prefixes are kept, so this is the promise.
    expect(apiUrl('/v1/ws', 'https://example.com/axon').toString()).toBe(
      'https://example.com/axon/v1/ws',
    )
  })

  it('does not double the slash when the base has a trailing one', () => {
    expect(apiUrl('/v1/ws', 'https://example.com/axon/').toString()).toBe(
      'https://example.com/axon/v1/ws',
    )
  })

  it('is unchanged for a root base', () => {
    expect(apiUrl('/v1/ws', 'https://example.com').toString()).toBe(
      'https://example.com/v1/ws',
    )
  })
})

describe('what a person actually types', () => {
  it('accepts host:port, the natural form for a LAN server', () => {
    // A bare colon is not a scheme. Detecting one by `:` alone read
    // `localhost:8080` as an unsupported `localhost:` scheme and rejected it.
    expect(normalizeServerUrl('localhost:8080')).toBe('https://localhost:8080')
  })

  it('rejects a base carrying credentials', () => {
    // Reachable by accident once a bare colon no longer reads as a scheme:
    // `https://mailto:a@b.c` parses as user `mailto`, password `a`, host `b.c`.
    // Credentials would ride into every request and the cache namespace.
    expect(normalizeServerUrl('mailto:a@b.c')).toBeNull()
  })
})
