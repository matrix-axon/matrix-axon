import type { CacheArea, CacheStore } from './cache-store'
import type { RoomDto } from './room-list'

/**
 * The room-list half of the durable cache (ADR 0085 phase 2), as the narrow
 * port `createRoomsStore` consumes. The store knows nothing about IndexedDB,
 * cache keys, or the enabled setting; all of that is assembled in `services.ts`.
 */
export interface RoomListCache {
  /** The cached rows, or `undefined` for a cold, disabled, or unusable cache. */
  read(): Promise<readonly RoomDto[] | undefined>
  /** Persist the rows a settled refresh returned. Fire-and-forget. */
  write(rooms: readonly RoomDto[]): void
}

const AREA: CacheArea = 'rooms'

/** Bumped when the record below changes shape incompatibly. */
const RECORD_VERSION = 1

interface RoomListRecord {
  version: number
  savedAt: number
  rooms: RoomDto[]
}

/**
 * The fields that reach disk, as an explicit allow-list rather than the DTO
 * whole.
 *
 * This is the mechanism behind phase 2's "metadata only" claim, not a
 * restatement of it: `RoomDto` holds no message text *today*, and a projection
 * means a field added to it later cannot silently carry message content onto
 * disk ahead of phase 3's opt-in. The cost is that a genuinely new metadata
 * field goes uncached until it is listed here — a row that is missing it until
 * the refresh lands, which is the same correction every cached row already
 * takes.
 *
 * The latest-message preview is deliberately absent and is not a `RoomDto`
 * field at all: it is fetched per room (`fetchPreview`) and it *is* message
 * body text, so caching it belongs to phase 3's opt-in, not here.
 */
function persistable(room: RoomDto): RoomDto {
  return {
    account_id: room.account_id,
    account_user_id: room.account_user_id,
    room_id: room.room_id,
    name: room.name ?? null,
    topic: room.topic ?? null,
    canonical_alias: room.canonical_alias ?? null,
    avatar_url: room.avatar_url ?? null,
    room_type: room.room_type ?? null,
    last_event_id: room.last_event_id ?? null,
    last_activity_ts: room.last_activity_ts,
    notification_count: count(room.notification_count),
    highlight_count: count(room.highlight_count),
  }
}

/** Unread counts are normalized, matching `countFromRoom` in the store. */
function count(value: unknown): number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value > 0
    ? value
    : 0
}

/**
 * A row is usable only if its **identity and sort key** are sound — the fields
 * without which it cannot be keyed, ranked, or routed to.
 *
 * Deliberately no stricter than that. The counts are validated by `count()`
 * rather than gating the row, because the store already tolerates a missing or
 * nonsense count and an over-strict check here fails *closed on a whole list*:
 * the e2e mock omits both counts, and the first version of this rejected every
 * row it wrote, silently, with the cache appearing to work.
 */
function validRoom(value: unknown): value is RoomDto {
  if (typeof value !== 'object' || value === null) {
    return false
  }
  const room = value as Partial<RoomDto>
  return (
    typeof room.account_id === 'string' &&
    typeof room.account_user_id === 'string' &&
    typeof room.room_id === 'string' &&
    typeof room.last_activity_ts === 'number' &&
    Number.isFinite(room.last_activity_ts)
  )
}

export interface RoomListCacheOptions {
  cache: CacheStore
  /**
   * The key every record is stored under — see `cacheNamespace`.
   *
   * A **function**, re-evaluated per operation, not a promise settled once at
   * startup. Token-paste sign-in does not reload the document, so a graph built
   * while signed out would otherwise hold `null` for the life of the session
   * and write nothing at all — leaving the launch *after* a sign-in still cold,
   * which on a phone is a launch the user actually sees. Re-evaluating also
   * means a token replaced mid-session moves to its own namespace immediately
   * rather than at the next reload.
   */
  namespace: () => Promise<string | null>
  /** The user's setting, read at each call so a toggle takes effect at once. */
  enabled: () => boolean
}

export function createRoomListCache({
  cache,
  namespace,
  enabled,
}: RoomListCacheOptions): RoomListCache {
  async function key(): Promise<string | null> {
    if (!enabled()) {
      return null
    }
    const resolved = await namespace()
    // Re-check after the await, not only before it. Deriving the namespace is
    // an async digest, and the setting can be turned off inside that window.
    return enabled() ? resolved : null
  }

  return {
    async read() {
      const cacheKey = await key()
      if (cacheKey === null) {
        return undefined
      }
      const record = await cache.read<RoomListRecord>(AREA, cacheKey)
      if (
        record === undefined ||
        record === null ||
        typeof record !== 'object' ||
        record.version !== RECORD_VERSION ||
        !Array.isArray(record.rooms)
      ) {
        return undefined
      }
      // A record with some unusable rows keeps the rest: a partial list is
      // corrected by the refresh already on its way, and dropping the lot
      // would trade a good list for a blank pane.
      const rooms = record.rooms.filter(validRoom)
      return rooms.length === 0 ? undefined : rooms
    },

    write(rooms) {
      // Captured synchronously, before anything can be awaited: any `clear()`
      // that begins from here on must beat this write, not lose to it.
      const generation = cache.generation
      void (async () => {
        const cacheKey = await key()
        if (cacheKey === null || cache.generation !== generation) {
          return
        }
        const record: RoomListRecord = {
          version: RECORD_VERSION,
          savedAt: Date.now(),
          rooms: rooms.map(persistable),
        }
        await cache.write(AREA, cacheKey, record)
        if (cache.generation !== generation) {
          // A wipe landed while this write was committing. Undo it rather than
          // leave a record the user asked to have deleted.
          await cache.drop(AREA, cacheKey)
          return
        }
        // One reader, one record. A token replaced without a sign-out leaves
        // the previous reader's rows behind — unreadable, since the key no
        // longer matches, but there is no reason to keep them on disk.
        for (const stale of await cache.keys(AREA)) {
          if (stale !== cacheKey) {
            await cache.drop(AREA, stale)
          }
        }
      })()
    },
  }
}
