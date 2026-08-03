import type { ApiClient } from '../api/client'
import type { MediaService } from '../media/media-service'
import { createTimelineStore, type TimelineStore } from './timeline'

/**
 * How many rooms keep a warm timeline store. The same cap ADR 0085 sets for
 * the persisted timeline cache, so a room's warm store and (from phase 3) its
 * persisted page fall out of the LRU together.
 *
 * The ceiling is bounded arithmetic rather than a guess: a store holds at most
 * `RETAINED_EVENT_LIMIT` (600) events at the 806 B/event the ADR measured on a
 * production account, so 30 rooms *all* scrolled fully back is ~14 MB, and the
 * realistic case is far below it — the median room in that account holds ~20
 * events, putting 30 rooms at well under a megabyte. No DOM is retained: rows
 * belong to the mounted `RoomPage`, not to the store.
 */
export const TIMELINE_STORE_LIMIT = 30

/**
 * Warm timeline stores, keyed by account and room (ADR 0085 phase 1).
 *
 * `RoomPage` used to `useMemo` a store on `[api, media, accountId, roomId]`,
 * so leaving a room and returning **within one session** threw away every
 * event already fetched and re-paint went back through "Loading timeline…".
 * That is the room switch a desktop tab pays over and over. Holding the store
 * instead makes re-entry paint immediately and reconcile in place.
 *
 * A cached store is *stale by construction*: live frames only reach the
 * mounted room (`RoomPage`'s subscription), so anything cached here stopped
 * receiving them the moment the user left. Re-entry must therefore gap-fill —
 * `loadLatest()` over a populated slice is `refreshHead`'s merge (ADR 0061),
 * which is exactly the reconciliation a slice that sat idle needs.
 */
export interface TimelineStoreCache {
  /**
   * The warm store for a room, or a fresh one. Marks it most recently used;
   * the caller still owns the gap-fill (see the note above).
   */
  acquire(accountId: string, roomId: string): TimelineStore
  /** Drop every store, unsent echoes included. Sign-out only. */
  clear(): void
  /**
   * True while any warm store holds a send this client has not placed —
   * pending, or failed and retryable. A media upload in flight is one of these
   * (its echo carries the `File`), so this covers attachments too. The
   * auto-refresh policy reads it to decide whether reloading would destroy
   * work: an unsent echo lives only in memory, so a reload would take it.
   */
  readonly hasUnsentWork: boolean
  /** Warm room count, for tests and diagnostics. */
  readonly size: number
}

/**
 * A store holding a send this client has not placed — pending, or failed and
 * retryable. Evicting one would silently destroy the user's unsent message
 * (and, for a media send, the `File` behind it), which is the repo-wide
 * "user-entered text must survive" guardrail applied to eviction.
 */
function holdsUnsentWork(store: TimelineStore): boolean {
  // `peek()`, not `.value`: `acquire` runs inside a component's `useMemo`, and
  // subscribing the render to every room's slice would be a re-render loop.
  return store.events.peek().some((event) => event.localEcho !== undefined)
}

/**
 * A slice deliberately parked in history — a date jump, or a scroll-back that
 * trimmed its newest end — cannot be gap-filled: `refreshHead` refuses to
 * move it (WCR-05, and rightly, since replacing it would teleport a reader
 * mid-session). Reusing one as-is would therefore re-open the room where the
 * reader left off *and* never show what arrived since, so re-entry rebuilds.
 *
 * A parked store is still worth keeping when it holds unsent work, because
 * that work cannot be rebuilt — but keeping it is not the same as reusing its
 * *slice*. `resumeAtHead()` is the difference: it drops the parked history and
 * cursor, keeps the echoes, and lets the following head load land on the newest
 * page. Returning `true` here without that call is the bug this comment exists
 * to prevent: the room would open on old history, missing everything since.
 */
function reusable(store: TimelineStore): boolean {
  return store.atEnd.peek() || holdsUnsentWork(store)
}

export function createTimelineStoreCache(
  api: ApiClient,
  media: MediaService,
  limit: number = TIMELINE_STORE_LIMIT,
): TimelineStoreCache {
  /**
   * Insertion order is LRU order: a hit re-inserts, so the oldest live entry
   * is always first. Composite keys join on `'\0'` (never a printable
   * character), as `media-service.ts` does.
   */
  const stores = new Map<string, TimelineStore>()

  function evict(exempt: string): void {
    for (const [key, store] of stores) {
      if (stores.size <= limit) {
        return
      }
      // The store just handed out is never a victim, and unsent work pins its
      // store. With everything pinned the cache runs over the cap rather than
      // dropping a message — bounded by how many rooms have a send in flight.
      if (key === exempt || holdsUnsentWork(store)) {
        continue
      }
      stores.delete(key)
      store.dispose()
    }
  }

  return {
    /**
     * Called from a component's `useMemo`, so the signal writes below happen
     * *during* render. That is sound under Preact's synchronous render: the
     * writes land before this render's subscriptions are collected, so they
     * cannot re-enter it, and they touch only the store being handed out. It
     * would stop being sound under an interruptible or replayed render, where
     * a discarded render attempt would still have mutated the store — if that
     * ever arrives, move both writes into an effect keyed on the room.
     *
     * They are not merely deferrable: a warm store must be at the head, and
     * free of the last visit's error, *before* the first paint reads it.
     */
    acquire(accountId, roomId) {
      const key = `${accountId}\0${roomId}`
      const warm = stores.get(key)
      if (warm !== undefined && reusable(warm)) {
        stores.delete(key)
        stores.set(key, warm)
        if (!warm.atEnd.peek()) {
          // Parked in history, and only kept because it holds unsent work.
          // Rebuild at the head: the echo survives, and the room opens on
          // current messages rather than wherever the reader had jumped to.
          warm.resumeAtHead()
        }
        // A banner left over from the last visit is not this visit's news; the
        // failed echo behind it, if any, still renders its own row status.
        warm.error.value = null
        return warm
      }
      if (warm !== undefined) {
        stores.delete(key)
        // Safe: `reusable` said no, so it holds no unsent work.
        warm.dispose()
      }
      const store = createTimelineStore(api, media, accountId, roomId)
      stores.set(key, store)
      evict(key)
      return store
    },

    clear() {
      for (const store of stores.values()) {
        store.dispose()
      }
      stores.clear()
    },

    get hasUnsentWork() {
      return [...stores.values()].some(holdsUnsentWork)
    },

    get size() {
      return stores.size
    },
  }
}
