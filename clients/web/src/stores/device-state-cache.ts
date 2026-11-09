import type { CacheArea, CacheStore } from './cache-store'

/**
 * A device-state scope as the server last described it (ADR 0131): the merged
 * entries as of `cursor`, the cursor that read returned.
 */
export interface CachedDeviceStateScope {
  cursor: string
  entries: ReadonlyMap<string, unknown>
}

/**
 * The device-state half of the durable cache (ADR 0131, on ADR 0085's store),
 * as the narrow port `createDeviceStateStore` consumes. Which namespaces are
 * persisted, the cache key, and the enabled setting all live here.
 */
export interface DeviceStateCache {
  /** The persisted scope, or `undefined` for one not cached or not cacheable. */
  read(
    accountId: string,
    namespace: string,
  ): Promise<CachedDeviceStateScope | undefined>
  /** Drop a scope's record, whose cursor the server refused. Fire-and-forget. */
  forget(accountId: string, namespace: string): void
  /** Persist a scope's server view. Fire-and-forget. */
  write(
    accountId: string,
    namespace: string,
    scope: CachedDeviceStateScope,
  ): void
}

/** Shared with telemetry, which keeps its one record under a fixed key. */
const AREA: CacheArea = 'meta'

/** Bumped when the record below changes shape incompatibly. */
const RECORD_VERSION = 1

/** Every key this module writes starts with this. */
const KEY_PREFIX = `device-state.v${RECORD_VERSION}\0`

/**
 * The namespaces that reach disk. Read markers are event ids and timestamps,
 * metadata of the room list's class. `drafts` is absent on purpose: a draft is
 * message text, and ADR 0085 keeps plaintext off the disk by default.
 */
const PERSISTED_NAMESPACES: ReadonlySet<string> = new Set([
  'read_markers',
  'thread_read_markers',
])

interface DeviceStateRecord {
  version: number
  savedAt: number
  cursor: string
  entries: Record<string, unknown>
}

export interface DeviceStateCacheOptions {
  cache: CacheStore
  /** The reader's cache namespace, re-evaluated per operation — see `cacheNamespace`. */
  namespace: () => Promise<string | null>
  /** The room-list cache setting, read at each call. */
  enabled: () => boolean
}

export function createDeviceStateCache({
  cache,
  namespace,
  enabled,
}: DeviceStateCacheOptions): DeviceStateCache {
  /** The reader prefix whose previous-reader sweep has finished. */
  let swept: string | null = null

  async function readerPrefix(): Promise<string | null> {
    if (!enabled()) {
      return null
    }
    const resolved = await namespace()
    // Re-checked after the await: the digest is async, and the setting can be
    // turned off inside that window.
    return enabled() && resolved !== null ? `${KEY_PREFIX}${resolved}\0` : null
  }

  return {
    async read(accountId, scopeNamespace) {
      if (!PERSISTED_NAMESPACES.has(scopeNamespace)) {
        return undefined
      }
      const prefix = await readerPrefix()
      if (prefix === null) {
        return undefined
      }
      const record = await cache.read<DeviceStateRecord>(
        AREA,
        `${prefix}${accountId}\0${scopeNamespace}`,
      )
      if (
        typeof record !== 'object' ||
        record === null ||
        record.version !== RECORD_VERSION ||
        typeof record.cursor !== 'string' ||
        record.cursor === '' ||
        typeof record.entries !== 'object' ||
        record.entries === null
      ) {
        return undefined
      }
      // Values are not validated here: every reader parses them already
      // (`parseMarker`, `parseThreadReadMarker`), as it does a server value.
      return {
        cursor: record.cursor,
        entries: new Map(Object.entries(record.entries)),
      }
    },

    forget(accountId, scopeNamespace) {
      if (!PERSISTED_NAMESPACES.has(scopeNamespace)) {
        return
      }
      void (async () => {
        const prefix = await readerPrefix()
        if (prefix !== null) {
          await cache.drop(AREA, `${prefix}${accountId}\0${scopeNamespace}`)
        }
      })()
    },

    write(accountId, scopeNamespace, scope) {
      if (!PERSISTED_NAMESPACES.has(scopeNamespace)) {
        return
      }
      // Captured before any await, so a `clear()` that starts from here on
      // beats this write (see `CacheStore.generation`).
      const generation = cache.generation
      const record: DeviceStateRecord = {
        version: RECORD_VERSION,
        savedAt: Date.now(),
        cursor: scope.cursor,
        entries: Object.fromEntries(scope.entries),
      }
      void (async () => {
        const prefix = await readerPrefix()
        if (prefix === null || cache.generation !== generation) {
          return
        }
        const key = `${prefix}${accountId}\0${scopeNamespace}`
        await cache.write(AREA, key, record)
        if (cache.generation !== generation) {
          // A wipe landed while this was committing; undo it.
          await cache.drop(AREA, key)
          return
        }
        // A token replaced without a sign-out leaves the previous reader's
        // records behind, unreadable under the new key. Drop them, but only
        // while no wipe has started: after one, a key outside this prefix may
        // be the next session's own record (review). Once per reader: later
        // writes under the same prefix would find nothing to drop (review).
        if (swept === prefix) {
          return
        }
        const keys = await cache.keys(AREA)
        for (const stale of keys) {
          if (cache.generation !== generation) {
            return
          }
          if (stale.startsWith(KEY_PREFIX) && !stale.startsWith(prefix)) {
            await cache.drop(AREA, stale)
          }
        }
        swept = prefix
      })()
    },
  }
}
