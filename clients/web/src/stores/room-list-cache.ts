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
  /**
   * The server's list as of its cursor (ADR 0134), or `undefined` for none,
   * a disabled cache, or an unusable record.
   */
  readServerView(): Promise<RoomListServerView | undefined>
  /**
   * Persist the server's list and its cursor. Fire-and-forget. With
   * `unlessSaved`, skip the write when this reader's record is already on
   * disk in this cache generation: a delta that changed nothing need not
   * rewrite it, but one arriving after the cache was off or cleared must.
   */
  writeServerView(
    view: RoomListServerView,
    options?: { unlessSaved?: boolean },
  ): void
  /** Drop the server-view record, whose cursor is no use. Fire-and-forget. */
  forgetServerView(): void
}

/**
 * The room list as the server last described it (ADR 0134): every listed
 * room as of `cursor`, from responses only. It never holds a live patch or a
 * local write, so a delta merged into it is the server's list as of the
 * delta's cursor.
 */
export interface RoomListServerView {
  cursor: string
  rooms: readonly RoomDto[]
}

const AREA: CacheArea = 'rooms'

/**
 * The server view lives in `meta`, not beside the painted rows in `rooms`:
 * every painted write prunes the `rooms` area down to its own key.
 */
const SERVER_VIEW_AREA: CacheArea = 'meta'

/** Bumped when the server-view record changes shape incompatibly. */
const SERVER_VIEW_VERSION = 1

/** Every server-view key starts with this; the reader's namespace follows. */
const SERVER_VIEW_PREFIX = `room-list-server.v${SERVER_VIEW_VERSION}\0`

interface RoomListServerViewRecord {
  version: number
  savedAt: number
  cursor: string
  rooms: RoomDto[]
}

/**
 * Bumped when the record below changes shape incompatibly.
 * Version 2 stores `tags`. A version 1 record has none, so restoring it
 * would hide favourites until the next refresh. Those records are discarded.
 */
const RECORD_VERSION = 2

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
 *
 * It is also the room list's definition of an unchanged row (ADR 0134's
 * delta merge compares projections). So a field added to `RoomDto` fails to
 * compile here until it is either listed or named in
 * {@link UnpersistedRoomField}; left out silently, a server-side change to it
 * alone would be ignored by the merge.
 */
export function persistable(room: RoomDto): RoomDto {
  return {
    account_id: room.account_id,
    account_user_id: room.account_user_id,
    room_id: room.room_id,
    name: room.name ?? null,
    topic: room.topic ?? null,
    canonical_alias: room.canonical_alias ?? null,
    display_name: room.display_name ?? null,
    avatar_url: room.avatar_url ?? null,
    room_type: room.room_type ?? null,
    last_event_id: room.last_event_id ?? null,
    last_activity_ts: room.last_activity_ts,
    notification_count: count(room.notification_count),
    highlight_count: count(room.highlight_count),
    is_direct: room.is_direct === true,
    tags: persistableTags(room.tags),
  } satisfies Required<Omit<RoomDto, UnpersistedRoomField>>
}

/**
 * `RoomDto` fields deliberately kept off disk, and so out of the delta
 * merge's comparison. None today.
 */
type UnpersistedRoomField = never

function persistableTags(value: RoomDto['tags']): NonNullable<RoomDto['tags']> {
  if (!Array.isArray(value)) {
    return []
  }
  const tags: NonNullable<RoomDto['tags']> = []
  for (const tag of value) {
    if (
      typeof tag !== 'object' ||
      tag === null ||
      typeof tag.name !== 'string'
    ) {
      continue
    }
    if (typeof tag.order === 'number' && Number.isFinite(tag.order)) {
      tags.push({ name: tag.name, order: tag.order })
    } else if (tag.order === null) {
      tags.push({ name: tag.name, order: null })
    } else {
      tags.push({ name: tag.name })
    }
  }
  return tags
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
  /**
   * Server-view writes and drops, in the order they were asked for. Each
   * awaits the async namespace digest before it touches the disk, so without
   * this a drop asked for just before a write (a refused cursor, then the
   * full read that replaces it) could land after the write and delete it.
   */
  let serverViewQueue: Promise<void> = Promise.resolve()
  function enqueueServerView<T>(op: () => Promise<T>): Promise<T> {
    const run = serverViewQueue.then(op, op)
    serverViewQueue = run.then(
      () => {},
      () => {},
    )
    return run
  }
  /** Queue a write or drop. Fire-and-forget: a failed one costs a full read. */
  function queueServerViewWrite(op: () => Promise<void>): void {
    enqueueServerView(op).catch(() => {})
  }

  /**
   * The generation and key of the server-view record known to be on disk, or
   * `null`. A wipe bumps the generation, and a disabled cache never sets it,
   * so a mismatch means the record has to be written again.
   */
  let savedServerView: string | null = null
  const savedAs = (generation: number, ownKey: string) =>
    `${generation}\0${ownKey}`

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

    readServerView() {
      // In the queue: after any write or drop already asked for, so the read
      // sees it, and before any asked for later, so a drop cannot land
      // between this read and the saved mark it sets.
      return enqueueServerView(async () => {
        const generation = cache.generation
        const cacheKey = await key()
        if (cacheKey === null) {
          return undefined
        }
        const ownKey = `${SERVER_VIEW_PREFIX}${cacheKey}`
        const record = await cache.read<RoomListServerViewRecord>(
          SERVER_VIEW_AREA,
          ownKey,
        )
        if (
          typeof record !== 'object' ||
          record === null ||
          record.version !== SERVER_VIEW_VERSION ||
          typeof record.cursor !== 'string' ||
          record.cursor === '' ||
          !Array.isArray(record.rooms)
        ) {
          return undefined
        }
        // All or nothing, unlike the painted rows. A delta is merged into this
        // list, and a list missing a row would fail the merge's count check
        // and cost a full read anyway; saying so here is clearer.
        if (!record.rooms.every(validRoom)) {
          return undefined
        }
        if (cache.generation === generation) {
          savedServerView = savedAs(generation, ownKey)
        }
        return { cursor: record.cursor, rooms: record.rooms }
      })
    },

    writeServerView(view, options = {}) {
      const generation = cache.generation
      queueServerViewWrite(async () => {
        const cacheKey = await key()
        if (cacheKey === null || cache.generation !== generation) {
          return
        }
        const ownKey = `${SERVER_VIEW_PREFIX}${cacheKey}`
        if (
          options.unlessSaved === true &&
          savedServerView === savedAs(generation, ownKey)
        ) {
          return
        }
        const record: RoomListServerViewRecord = {
          version: SERVER_VIEW_VERSION,
          savedAt: Date.now(),
          cursor: view.cursor,
          rooms: view.rooms.map(persistable),
        }
        await cache.write(SERVER_VIEW_AREA, ownKey, record)
        if (cache.generation !== generation) {
          await cache.drop(SERVER_VIEW_AREA, ownKey)
          return
        }
        savedServerView = savedAs(generation, ownKey)
        // As for the painted rows: one reader, one record. Only this module's
        // keys, since `meta` is shared.
        for (const stale of await cache.keys(SERVER_VIEW_AREA)) {
          if (stale.startsWith(SERVER_VIEW_PREFIX) && stale !== ownKey) {
            await cache.drop(SERVER_VIEW_AREA, stale)
          }
        }
      })
    },

    forgetServerView() {
      queueServerViewWrite(async () => {
        savedServerView = null
        const cacheKey = await key()
        if (cacheKey !== null) {
          await cache.drop(SERVER_VIEW_AREA, `${SERVER_VIEW_PREFIX}${cacheKey}`)
        }
      })
    },
  }
}
