/**
 * Rooms whose per-room stores stay warm across switches. The timeline cache's
 * cap (`TIMELINE_STORE_LIMIT`), so a room's timeline, members and threads fall
 * out together.
 */
export const ROOM_STORE_LIMIT = 30

/**
 * Warm per-room stores, keyed by account and room, least recently acquired
 * evicted first — the shape ADR 0085 phase 1 gave timelines, for the stores
 * that need nothing more: no unsent work to pin, and no slice that can be
 * parked out of reach. `RoomPage` used to build these per mount, so every
 * return visit re-fetched what it had just shown. A warm store paints what it
 * holds, and the page's mount effect refreshes it in place.
 *
 * The timeline cache stays separate (`timeline-cache.ts`): it pins stores
 * holding a send and refuses a slice parked in history, which this does not.
 */
export interface RoomStoreCache<T> {
  /** The warm store for a room, or a fresh one. Marks it most recently used. */
  acquire(accountId: string, roomId: string): T
  /** Drop one account's stores, releasing each: logged out or deleted. */
  forgetAccount(accountId: string): void
  /** Drop every store, releasing each. Sign-out only. */
  clear(): void
  /** Warm room count, for tests and diagnostics. */
  readonly size: number
}

export function createRoomStoreCache<T>(
  create: (accountId: string, roomId: string) => T,
  options: {
    limit?: number
    /** Runs on each store the cache lets go of: evicted, or cleared. */
    release?: (store: T) => void
  } = {},
): RoomStoreCache<T> {
  const limit = options.limit ?? ROOM_STORE_LIMIT
  // Insertion order is LRU order: a hit re-inserts, so the oldest is first.
  // Composite keys join on '\0', as `timeline-cache.ts` does.
  const stores = new Map<string, T>()
  return {
    acquire(accountId, roomId) {
      const key = `${accountId}\0${roomId}`
      const warm = stores.get(key)
      if (warm !== undefined) {
        stores.delete(key)
        stores.set(key, warm)
        return warm
      }
      const store = create(accountId, roomId)
      stores.set(key, store)
      for (const [oldest, evicted] of stores) {
        if (stores.size <= limit) {
          break
        }
        stores.delete(oldest)
        options.release?.(evicted)
      }
      return store
    },

    forgetAccount(accountId) {
      const prefix = `${accountId}\0`
      for (const [key, store] of [...stores]) {
        if (key.startsWith(prefix)) {
          stores.delete(key)
          options.release?.(store)
        }
      }
    },

    clear() {
      const all = [...stores.values()]
      stores.clear()
      for (const store of all) {
        options.release?.(store)
      }
    },

    get size() {
      return stores.size
    },
  }
}
