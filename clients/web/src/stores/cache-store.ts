/**
 * The durable content cache (ADR 0085 phase 2) — a consumer-owned port with an
 * IndexedDB adapter, in the shape ADR 0021 asks for and `Storage` injection
 * already follows elsewhere in this package.
 *
 * **Every operation is best-effort.** A failed open (private browsing, storage
 * denied), a quota rejection, or a malformed record degrades silently to the
 * uncached behavior — the same policy `saveTitleCache` applies, and for the
 * same reason: a cache write must never surface an error over a successful
 * load. Nothing here rejects; failures resolve to `undefined` or to nothing.
 *
 * IndexedDB rather than `localStorage` on two grounds. **Quota**: the room list
 * alone is 660 KB on the production account ADR 0085 measured, against a ~5 MB
 * origin-wide `localStorage` budget already shared with credentials and
 * settings. **Synchrony**: `localStorage` blocks the main thread by
 * construction, whatever it costs.
 */

/** Object stores in the cache database. `timelines` is phase 3's. */
export type CacheArea = 'rooms' | 'timelines' | 'meta'

export const CACHE_AREAS: readonly CacheArea[] = ['rooms', 'timelines', 'meta']

export const CACHE_DATABASE_NAME = 'axon-cache'

/**
 * Bumped for any incompatible change to a cached record's shape. A mismatch
 * **drops** the database rather than migrating it: the cache is an
 * optimization, and re-fetching is always correct.
 */
export const CACHE_SCHEMA_VERSION = 1

export interface CacheStore {
  read<T>(area: CacheArea, key: string): Promise<T | undefined>
  write<T>(area: CacheArea, key: string, value: T): Promise<void>
  drop(area: CacheArea, key: string): Promise<void>
  /** Every key in one area, so a consumer can prune records it no longer owns. */
  keys(area: CacheArea): Promise<string[]>
  /** Drop every area. Sign-out and token change only — see `connectCacheReset`. */
  clear(): Promise<void>
  /**
   * Bumped **synchronously** by every `clear()`, before any of its async work.
   *
   * A wipe is a privacy barrier, and a write that was already on its way must
   * not be allowed over the top of it: writers capture this before they start
   * and re-check it immediately before committing, so a `clear()` that begins
   * at any point in between wins. Without it, `connectCacheSetting`'s
   * unawaited `clear()` races the write from a refresh that is still settling,
   * and "turning this off erases what was stored" is not quite true.
   */
  readonly generation: number
}

/**
 * The in-memory adapter tests run against, so no test needs `fake-indexeddb`
 * and none inherits jsdom's IDB quirks. Values are structured-cloned on the way
 * in and out, matching what IndexedDB does — a test that mutates what it wrote
 * must not be able to change what it reads back.
 */
export function createMemoryCacheStore(): CacheStore {
  let generation = 0
  const areas = new Map<CacheArea, Map<string, unknown>>(
    CACHE_AREAS.map((area) => [area, new Map<string, unknown>()]),
  )
  const area = (name: CacheArea): Map<string, unknown> => {
    const existing = areas.get(name)
    if (existing !== undefined) {
      return existing
    }
    const created = new Map<string, unknown>()
    areas.set(name, created)
    return created
  }

  return {
    read<T>(name: CacheArea, key: string): Promise<T | undefined> {
      const value = area(name).get(key)
      return Promise.resolve(
        value === undefined ? undefined : (structuredClone(value) as T),
      )
    },
    write<T>(name: CacheArea, key: string, value: T): Promise<void> {
      try {
        area(name).set(key, structuredClone(value))
      } catch {
        // Uncloneable value: the production adapter would reject it too.
      }
      return Promise.resolve()
    },
    drop(name: CacheArea, key: string): Promise<void> {
      area(name).delete(key)
      return Promise.resolve()
    },
    keys(name: CacheArea): Promise<string[]> {
      return Promise.resolve([...area(name).keys()])
    },
    clear(): Promise<void> {
      generation += 1
      for (const store of areas.values()) {
        store.clear()
      }
      return Promise.resolve()
    },
    get generation() {
      return generation
    },
  }
}

/**
 * The production adapter: one database, one object store per area.
 *
 * The open is memoized, including its failure — a browser that refused once
 * (private browsing, a blocked upgrade from another tab) will refuse for the
 * life of the document, and retrying per read would only cost time.
 */
export function createIndexedDbCacheStore(
  factory: IDBFactory | undefined = globalThis.indexedDB,
): CacheStore {
  let opening: Promise<IDBDatabase | null> | null = null
  let generation = 0

  function open(): Promise<IDBDatabase | null> {
    opening ??= new Promise<IDBDatabase | null>((resolve) => {
      if (factory === undefined) {
        resolve(null)
        return
      }
      let request: IDBOpenDBRequest
      try {
        request = factory.open(CACHE_DATABASE_NAME, CACHE_SCHEMA_VERSION)
      } catch {
        resolve(null)
        return
      }
      request.onupgradeneeded = () => {
        const db = request.result
        // Drop rather than migrate, per ADR 0085: an older shape is discarded
        // wholesale and rebuilt from the next refresh.
        for (const name of [...db.objectStoreNames]) {
          db.deleteObjectStore(name)
        }
        for (const name of CACHE_AREAS) {
          db.createObjectStore(name)
        }
      }
      request.onsuccess = () => {
        const db = request.result
        // Never hold another tab's upgrade open: a blocked upgrade there is a
        // tab that gets no cache at all, and this connection is disposable.
        db.onversionchange = () => db.close()
        resolve(db)
      }
      request.onerror = () => resolve(null)
      request.onblocked = () => resolve(null)
    })
    return opening
  }

  async function run<T>(
    area: CacheArea,
    mode: IDBTransactionMode,
    body: (store: IDBObjectStore) => IDBRequest,
  ): Promise<T | undefined> {
    const db = await open()
    if (db === null) {
      return undefined
    }
    return new Promise<T | undefined>((resolve) => {
      let request: IDBRequest
      try {
        const transaction = db.transaction(area, mode)
        // A quota rejection aborts the transaction rather than throwing here,
        // so both exits have to resolve — an unresolved promise would hang the
        // caller waiting on a cache.
        transaction.onabort = () => resolve(undefined)
        transaction.onerror = () => resolve(undefined)
        request = body(transaction.objectStore(area))
      } catch {
        resolve(undefined)
        return
      }
      request.onsuccess = () => resolve(request.result as T)
      request.onerror = () => resolve(undefined)
    })
  }

  return {
    async read<T>(area: CacheArea, key: string): Promise<T | undefined> {
      return await run<T>(area, 'readonly', (store) => store.get(key))
    },
    async write<T>(area: CacheArea, key: string, value: T): Promise<void> {
      await run(area, 'readwrite', (store) => store.put(value, key))
    },
    async drop(area: CacheArea, key: string): Promise<void> {
      await run(area, 'readwrite', (store) => store.delete(key))
    },
    async keys(area: CacheArea): Promise<string[]> {
      const keys = await run<IDBValidKey[]>(area, 'readonly', (store) =>
        store.getAllKeys(),
      )
      return (keys ?? []).filter(
        (key): key is string => typeof key === 'string',
      )
    },
    async clear(): Promise<void> {
      // Synchronously, before the first await: a writer that re-checks this
      // must see the wipe the instant it is asked for, not when it finishes.
      generation += 1
      for (const area of CACHE_AREAS) {
        await run(area, 'readwrite', (store) => store.clear())
      }
    },
    get generation() {
      return generation
    },
  }
}

/**
 * Ask the browser to exempt this origin's storage from routine eviction.
 *
 * The only lever against a non-installed Safari tab losing script-writable
 * storage after seven idle days. Granting is heuristic and silent refusal is
 * expected, so nothing may depend on the outcome — an evicted cache is a cold
 * start, which is exactly today's behavior.
 */
export function requestPersistentStorage(): void {
  try {
    void navigator.storage?.persist?.().catch(() => {})
  } catch {
    // No `navigator.storage` at all (older engines, some embedded webviews).
  }
}

/**
 * The namespace every cached record is keyed under: the server base URL, plus
 * a fingerprint of the bearer token in force.
 *
 * ADR 0085 specifies `(apiBaseUrl, account_id, …)`, but `GET /v1/rooms` is
 * **cross-account** — one list spanning every account on the server, with no
 * account id to key on. The token is what actually identifies the reader, and
 * keying on it closes a gap the sign-out wipe does not: a token replaced
 * *without* a sign-out (a re-paste, a shared machine) would otherwise hydrate
 * the previous reader's room list and show it until the refresh landed. A
 * different token simply misses the cache — a cold start, today's behavior.
 *
 * The digest is a **discriminator, not a secret**. It protects nothing that the
 * disk does not already give up: the token itself sits in `localStorage` beside
 * this database. That is why the non-cryptographic fallback below is
 * acceptable — `crypto.subtle` is absent on insecure origins (plain-http
 * development) and under jsdom, and a cache that silently stopped working
 * there would be worse than a weaker tag.
 */
export async function cacheNamespace(
  baseUrl: string,
  token: string | null,
): Promise<string | null> {
  if (token === null || token === '') {
    return null
  }
  return `${baseUrl}\0${await fingerprint(token)}`
}

async function fingerprint(token: string): Promise<string> {
  const subtle = globalThis.crypto?.subtle
  if (subtle !== undefined) {
    try {
      const digest = await subtle.digest(
        'SHA-256',
        new TextEncoder().encode(token),
      )
      return [...new Uint8Array(digest).slice(0, 8)]
        .map((byte) => byte.toString(16).padStart(2, '0'))
        .join('')
    } catch {
      // Fall through: some engines expose `subtle` and refuse to use it.
    }
  }
  return fnv1a(token)
}

/** FNV-1a, 32-bit. Only ever a fallback tag — see `cacheNamespace`. */
function fnv1a(value: string): string {
  let hash = 0x811c9dc5
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index)
    hash = Math.imul(hash, 0x01000193) >>> 0
  }
  return hash.toString(16).padStart(8, '0')
}
