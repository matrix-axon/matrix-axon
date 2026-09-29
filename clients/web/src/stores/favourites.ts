import {
  effect,
  signal,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import type { ApiClient } from '../api/client'
import { accountDataChange, type AccountDataChange } from '../api/frames'
import type { LiveConnection } from './live-connection'
import {
  FAVOURITE_TAG,
  favouriteOrder,
  isFavourite,
  roomKey,
  type RoomDto,
} from './room-list'

export { FAVOURITE_TAG, favouriteOrder, isFavourite }

/**
 * Below this gap a new order cannot be inserted and the visible favourites
 * are rebalanced. Shared with the TUI (`clients/tui/src/app/tags.rs`).
 */
export const ORDER_EPSILON = 1e-10

/** Keyboard repeat coalesces into one PUT. Pointer moves and the star do not. */
export const KEYBOARD_REORDER_DEBOUNCE_MS = 300

/**
 * A failed pin migration waits this long, then twice as long, up to
 * {@link MIGRATION_RETRY_MAX_MS}. A `rooms.refresh()` must not be the retry:
 * that signal moves on every reconnect and would PUT again immediately.
 */
export const MIGRATION_RETRY_BASE_MS = 1000
export const MIGRATION_RETRY_MAX_MS = 30_000

export type RoomTag = NonNullable<RoomDto['tags']>[number]

export interface FavouriteAssignment {
  key: string
  order: number
}

export type MovePlan =
  | { kind: 'noop' }
  | { kind: 'unpin'; key: string }
  | { kind: 'drop-local'; key: string }
  | { kind: 'assign'; assignments: FavouriteAssignment[] }

/** The slice of the room list favourites read and patch. */
export interface FavouriteRoomList {
  rooms: ReadonlySignal<readonly RoomDto[]>
  confirmed: ReadonlySignal<boolean>
  stale: ReadonlySignal<boolean>
  loading: ReadonlySignal<boolean>
  peekRooms(): readonly RoomDto[]
  assignRoomTags(key: string, tags: RoomTag[]): number
  restoreRoomTags(
    key: string,
    generation: number,
    tags: RoomTag[] | undefined,
  ): boolean
  applyAccountData(accountId: string, change: AccountDataChange): void
  refresh(): Promise<void>
}

export interface FavouriteStore {
  /** Session-only. Reveals the per-row move buttons; drag works either way. */
  reordering: Signal<boolean>
  error: Signal<string | null>
  /**
   * False when `key` is not in the room list, so nothing was written.
   * Callers that report the command's result must not treat that as success.
   */
  pin(key: string): boolean
  unpin(key: string): void
  /**
   * `toIndex` is an index into `visibleKeys`, using the same splice as
   * `settings.moveSpace`. Landing inside the favourite prefix writes a
   * midpoint `m.favourite`. Landing on or below the separator unpins.
   */
  move(
    key: string,
    toIndex: number,
    visibleKeys: readonly string[],
    favouriteCount: number,
    options?: { debounce?: boolean },
  ): void
  /** Arm the one-shot local-pin migration. The shared test graph does not. */
  start(): void
  resetSession(): void
}

export function splitRoomKey(
  key: string,
): { accountId: string; roomId: string } | null {
  const slash = key.indexOf('/')
  if (slash <= 0 || slash === key.length - 1) {
    return null
  }
  return { accountId: key.slice(0, slash), roomId: key.slice(slash + 1) }
}

/**
 * Parse an `m.tag` account-data `content` object. Malformed content is an
 * empty tag list — a bad frame must not drop the room. Port of
 * `parse_room_tags` in `clients/tui/src/api.rs`.
 */
export function parseRoomTags(content: unknown): RoomTag[] {
  if (
    typeof content !== 'object' ||
    content === null ||
    Array.isArray(content)
  ) {
    return []
  }
  const tags = (content as { tags?: unknown }).tags
  if (typeof tags !== 'object' || tags === null || Array.isArray(tags)) {
    return []
  }
  const parsed: RoomTag[] = []
  for (const [name, info] of Object.entries(tags)) {
    if (typeof info !== 'object' || info === null || !('order' in info)) {
      parsed.push({ name })
      continue
    }
    const raw = (info as { order?: unknown }).order
    if (typeof raw === 'number' && Number.isFinite(raw)) {
      parsed.push({ name, order: raw })
    } else if (raw === null) {
      parsed.push({ name, order: null })
    } else {
      parsed.push({ name })
    }
  }
  return parsed
}

/** Room ids listed in any array of an `m.direct` content object. */
export function roomIdsInDirectMap(content: unknown): Set<string> {
  const ids = new Set<string>()
  if (
    typeof content !== 'object' ||
    content === null ||
    Array.isArray(content)
  ) {
    return ids
  }
  for (const value of Object.values(content)) {
    if (!Array.isArray(value)) {
      continue
    }
    for (const entry of value) {
      if (typeof entry === 'string') {
        ids.add(entry)
      }
    }
  }
  return ids
}

export function withFavourite(
  tags: readonly RoomTag[] | undefined,
  order: number,
): RoomTag[] {
  const next = (tags ?? []).filter((tag) => tag.name !== FAVOURITE_TAG)
  next.push({ name: FAVOURITE_TAG, order })
  return next
}

export function withoutFavourite(
  tags: readonly RoomTag[] | undefined,
): RoomTag[] {
  return (tags ?? []).filter((tag) => tag.name !== FAVOURITE_TAG)
}

/**
 * Pin-to-top among this account's favourites. First favourite is `0.5`;
 * otherwise `min / 2` when that fits, else rebalance as `(i + 1) / (n + 1)`
 * with the target at index 0. Matches `pin_to_top_assignments` in
 * `clients/tui/src/app/tags.rs` — whichever client migrates first determines
 * the orders the other one will see.
 */
export function pinToTopAssignments(
  rooms: readonly RoomDto[],
  targetKey: string,
): FavouriteAssignment[] {
  const target = rooms.find((room) => roomKey(room) === targetKey)
  const accountId = target?.account_id ?? splitRoomKey(targetKey)?.accountId
  const others = rooms
    .filter(
      (room) =>
        accountId !== undefined &&
        room.account_id === accountId &&
        roomKey(room) !== targetKey &&
        isFavourite(room),
    )
    .sort((left, right) =>
      compareOrders(favouriteOrder(left), favouriteOrder(right)),
    )
  const min = others
    .map((room) => favouriteOrder(room))
    .find((order) => order !== null)
  if (min === null || min === undefined) {
    return [{ key: targetKey, order: 0.5 }]
  }
  if (min > ORDER_EPSILON) {
    return [{ key: targetKey, order: min / 2 }]
  }
  const denom = others.length + 2
  const assignments: FavouriteAssignment[] = [
    { key: targetKey, order: 1 / denom },
  ]
  others.forEach((room, index) => {
    assignments.push({ key: roomKey(room), order: (index + 2) / denom })
  })
  return assignments
}

/**
 * One-shot local-pin upload. Per account, preserve pin-list order and write
 * `(index + 1) / (n + 1)` so every order stays in `(0, 1)`. Matches
 * `migration_assignments` in `clients/tui/src/app/tags.rs`.
 */
export function migrationAssignments(
  pinned: readonly string[],
): FavouriteAssignment[] {
  const byAccount = new Map<string, string[]>()
  for (const key of pinned) {
    const parts = splitRoomKey(key)
    if (parts === null) {
      continue
    }
    const list = byAccount.get(parts.accountId) ?? []
    list.push(key)
    byAccount.set(parts.accountId, list)
  }
  const assignments: FavouriteAssignment[] = []
  for (const keys of byAccount.values()) {
    const denom = keys.length + 1
    keys.forEach((key, index) => {
      assignments.push({ key, order: (index + 1) / denom })
    })
  }
  return assignments
}

/**
 * Drag or Alt-arrow within the visible list. The favourite prefix is
 * `visibleKeys` sliced to `favouriteCount` (server favourites plus any
 * not-yet-migrated local pins). A drop past that prefix unpins.
 */
export function planFavouriteMove(
  key: string,
  toIndex: number,
  visibleKeys: readonly string[],
  favouriteCount: number,
  lookup: {
    orderOf: (key: string) => number | null
    isFavourite: (key: string) => boolean
  },
): MovePlan {
  const fromIndex = visibleKeys.indexOf(key)
  if (fromIndex < 0) {
    return { kind: 'noop' }
  }
  const without = visibleKeys.filter((candidate) => candidate !== key)
  const dest = Math.max(0, Math.min(toIndex, without.length))
  const result = [...without]
  result.splice(dest, 0, key)
  if (
    result.length === visibleKeys.length &&
    result.every((candidate, index) => candidate === visibleKeys[index])
  ) {
    return { kind: 'noop' }
  }
  const sourceInPrefix = fromIndex < favouriteCount
  const remainingPrefix = visibleKeys
    .slice(0, favouriteCount)
    .filter((candidate) => candidate !== key)
  const landedAt = result.indexOf(key)
  if (landedAt > remainingPrefix.length) {
    if (!sourceInPrefix) {
      return { kind: 'noop' }
    }
    return lookup.isFavourite(key)
      ? { kind: 'unpin', key }
      : { kind: 'drop-local', key }
  }
  const prefix = [
    ...remainingPrefix.slice(0, landedAt),
    key,
    ...remainingPrefix.slice(landedAt),
  ]
  return {
    kind: 'assign',
    assignments: ordersForInsertion(prefix, landedAt, lookup.orderOf),
  }
}

/**
 * Midpoint insert of `prefix[insertAt]`. No previous neighbour → `next / 2`
 * (or `0` when there is no next). No next → `(prev + 1) / 2`. Both →
 * the average. A missing neighbour order, a gap below {@link ORDER_EPSILON},
 * or an end that has already closed (`next` at 0, `prev` at 1) rebalances
 * the whole visible prefix as `(i + 1) / (n + 1)`.
 *
 * The open interval is required. `i / (n + 1)` puts the first row at 0, and
 * the next drag to the top then computes `0 / 2` and ties with it, so the
 * room does not land first. The same thing happens at 1.
 */
export function ordersForInsertion(
  prefix: readonly string[],
  insertAt: number,
  orderOf: (key: string) => number | null,
): FavouriteAssignment[] {
  const prevKey = insertAt > 0 ? prefix[insertAt - 1] : undefined
  const nextKey = prefix[insertAt + 1]
  const prev = prevKey === undefined ? null : orderOf(prevKey)
  const next = nextKey === undefined ? null : orderOf(nextKey)
  if (
    (prevKey !== undefined && prev === null) ||
    (nextKey !== undefined && next === null)
  ) {
    return rebalanceVisible(prefix, orderOf)
  }
  let order: number
  if (prev === null && next === null) {
    order = 0
  } else if (prev === null && next !== null) {
    if (!(next > ORDER_EPSILON)) {
      return rebalanceVisible(prefix, orderOf)
    }
    order = next / 2
  } else if (prev !== null && next === null) {
    if (!(1 - prev > ORDER_EPSILON)) {
      return rebalanceVisible(prefix, orderOf)
    }
    order = (prev + 1) / 2
  } else if (prev !== null && next !== null && next - prev < ORDER_EPSILON) {
    return rebalanceVisible(prefix, orderOf)
  } else if (prev !== null && next !== null) {
    order = (prev + next) / 2
  } else {
    return rebalanceVisible(prefix, orderOf)
  }
  const current = orderOf(prefix[insertAt])
  return current === order ? [] : [{ key: prefix[insertAt], order }]
}

export function rebalanceVisible(
  keys: readonly string[],
  orderOf: (key: string) => number | null,
): FavouriteAssignment[] {
  const denom = keys.length + 1
  const assignments: FavouriteAssignment[] = []
  keys.forEach((key, index) => {
    const order = (index + 1) / denom
    if (orderOf(key) !== order) {
      assignments.push({ key, order })
    }
  })
  return assignments
}

function compareOrders(left: number | null, right: number | null): number {
  if (left === null && right === null) {
    return 0
  }
  if (left === null) {
    return 1
  }
  if (right === null) {
    return -1
  }
  return left - right
}

function cloneTags(
  tags: readonly RoomTag[] | undefined,
): RoomTag[] | undefined {
  return tags?.map((tag) => ({ ...tag }))
}

interface DiffBatch {
  puts: FavouriteAssignment[]
  deletes: string[]
  keys: string[]
}

/**
 * Favourite writes and the one-shot `pinnedRooms` migration (ADR 0103).
 *
 * Optimistic tags land immediately. PUTs are serial and last-write-wins per
 * room: a failure restores the pre-burst tags only when no newer local edit
 * or ignored live frame has moved that room's generation. Tag frames for a
 * room with a write in flight are deferred to a refresh once the queue drains.
 * `m.direct` is applied immediately — it is not part of the tag write.
 */
export function createFavouriteStore(deps: {
  api: ApiClient
  live: LiveConnection
  rooms: FavouriteRoomList
  settings: { pinnedRooms: Signal<string[]> }
  accounts: {
    accounts: ReadonlySignal<readonly { account_id: string; state: string }[]>
    loading: ReadonlySignal<boolean>
    error: Signal<string | null>
  }
}): FavouriteStore {
  const { api, live, rooms, settings, accounts } = deps
  const reordering = signal(false)
  const error = signal<string | null>(null)
  const armed = signal(false)
  const migrationState = signal<'idle' | 'pending' | 'inflight' | 'done'>(
    'idle',
  )

  let sessionGeneration = 0
  let timer: ReturnType<typeof setTimeout> | null = null
  let flushWait: Promise<void> | null = null
  let skipPasses = 0
  let migrationFailures = 0
  let retryTimer: ReturnType<typeof setTimeout> | null = null
  let dirtyRefresh = false
  let captured = false
  let serverFavouriteAccounts: Set<string> | null = null
  const clientWrittenAccounts = new Set<string>()
  const baseline = new Map<string, RoomTag[] | undefined>()
  const generationByKey = new Map<string, number>()
  const pendingKeys = new Set<string>()
  const localDrop = new Set<string>()

  function tagsOf(key: string): RoomTag[] | undefined {
    return rooms.peekRooms().find((room) => roomKey(room) === key)?.tags
  }

  function orderOf(key: string): number | null {
    const tags = tagsOf(key)
    return tags === undefined ? null : favouriteOrder({ tags })
  }

  function keyIsFavourite(key: string): boolean {
    const tags = tagsOf(key)
    return tags !== undefined && isFavourite({ tags })
  }

  function removePinned(keys: readonly string[]): void {
    if (keys.length === 0) {
      return
    }
    const drop = new Set(keys)
    const current = settings.pinnedRooms.peek()
    const next = current.filter((key) => !drop.has(key))
    if (next.length !== current.length) {
      settings.pinnedRooms.value = next
    }
  }

  function captureIfReady(): void {
    if (captured || !rooms.confirmed.peek() || rooms.stale.peek()) {
      return
    }
    const ids = new Set<string>()
    for (const room of rooms.peekRooms()) {
      if (!isFavourite(room) || clientWrittenAccounts.has(room.account_id)) {
        continue
      }
      ids.add(room.account_id)
    }
    serverFavouriteAccounts = ids
    captured = true
  }

  function writeTags(key: string, tags: RoomTag[]): boolean {
    if (!rooms.peekRooms().some((room) => roomKey(room) === key)) {
      return false
    }
    const parts = splitRoomKey(key)
    const hadBaseline = baseline.has(key)
    if (!hadBaseline) {
      baseline.set(key, cloneTags(tagsOf(key)))
    }
    if (parts !== null && !captured) {
      clientWrittenAccounts.add(parts.accountId)
    }
    // Pending before the room signal moves. assignRoomTags notifies the
    // effect that drops baselines for keys that are not mid-edit, and that
    // runs before this function returns.
    const wasPending = pendingKeys.has(key)
    pendingKeys.add(key)
    const generation = rooms.assignRoomTags(key, tags)
    if (generation === 0) {
      if (!wasPending) {
        pendingKeys.delete(key)
      }
      if (!hadBaseline) {
        baseline.delete(key)
      }
      return false
    }
    generationByKey.set(key, generation)
    pendingKeys.add(key)
    if (error.peek() !== null) {
      error.value = null
    }
    return true
  }

  function collectDiff(): DiffBatch {
    const puts: FavouriteAssignment[] = []
    const deletes: string[] = []
    const keys: string[] = []
    for (const key of pendingKeys) {
      const base = baseline.get(key) ?? []
      const now = tagsOf(key) ?? []
      const baseHas = isFavourite({ tags: base })
      const nowHas = isFavourite({ tags: now })
      const baseOrder = favouriteOrder({ tags: base })
      const nowOrder = favouriteOrder({ tags: now })
      if (nowHas && nowOrder !== null && (!baseHas || baseOrder !== nowOrder)) {
        puts.push({ key, order: nowOrder })
        keys.push(key)
      } else if (baseHas && !nowHas) {
        deletes.push(key)
        keys.push(key)
      }
    }
    return { puts, deletes, keys }
  }

  function restoreKey(key: string, generation: number): void {
    if (generationByKey.get(key) !== generation) {
      return
    }
    const restored = rooms.restoreRoomTags(key, generation, baseline.get(key))
    if (!restored) {
      return
    }
    pendingKeys.delete(key)
    generationByKey.delete(key)
    baseline.delete(key)
    localDrop.delete(key)
  }

  /** Favourite presence and order, which is all a tag PUT or DELETE changes. */
  function sameFavourite(
    left: RoomTag[] | undefined,
    right: RoomTag[] | undefined,
  ): boolean {
    const leftTags = left ?? []
    const rightTags = right ?? []
    const leftHas = isFavourite({ tags: leftTags })
    const rightHas = isFavourite({ tags: rightTags })
    if (leftHas !== rightHas) {
      return false
    }
    if (!leftHas) {
      return true
    }
    return (
      favouriteOrder({ tags: leftTags }) === favouriteOrder({ tags: rightTags })
    )
  }

  /**
   * A pending key whose favourite state already matches the baseline has
   * nothing left to send. Dropping it is what lets a deferred tag frame
   * apply and a migration run. `localDrop` still clears the migrated pin.
   */
  function settleQuietKeys(): void {
    for (const key of [...pendingKeys]) {
      if (!sameFavourite(baseline.get(key), tagsOf(key))) {
        continue
      }
      pendingKeys.delete(key)
      generationByKey.delete(key)
      baseline.delete(key)
      if (localDrop.delete(key)) {
        removePinned([key])
      }
    }
  }

  function clearMigrationRetry(): void {
    if (retryTimer !== null) {
      clearTimeout(retryTimer)
      retryTimer = null
    }
    migrationFailures = 0
  }

  function scheduleMigrationRetry(): void {
    if (retryTimer !== null) {
      clearTimeout(retryTimer)
      retryTimer = null
    }
    migrationFailures += 1
    const delay = Math.min(
      MIGRATION_RETRY_MAX_MS,
      MIGRATION_RETRY_BASE_MS * 2 ** Math.min(migrationFailures - 1, 5),
    )
    retryTimer = setTimeout(() => {
      retryTimer = null
      void runMigration()
    }, delay)
  }

  async function send(batch: DiffBatch): Promise<boolean> {
    for (const key of batch.deletes) {
      const parts = splitRoomKey(key)
      if (parts === null) {
        return false
      }
      const result = await api.DELETE(
        '/v1/accounts/{account_id}/rooms/{room_id}/tags/{tag}',
        {
          params: {
            path: {
              account_id: parts.accountId,
              room_id: parts.roomId,
              tag: FAVOURITE_TAG,
            },
          },
        },
      )
      if (result.error !== undefined || !result.response.ok) {
        return false
      }
    }
    for (const assignment of batch.puts) {
      const parts = splitRoomKey(assignment.key)
      if (parts === null) {
        return false
      }
      const result = await api.PUT(
        '/v1/accounts/{account_id}/rooms/{room_id}/tags/{tag}',
        {
          params: {
            path: {
              account_id: parts.accountId,
              room_id: parts.roomId,
              tag: FAVOURITE_TAG,
            },
          },
          body: { order: assignment.order },
        },
      )
      if (result.error !== undefined || !result.response.ok) {
        return false
      }
    }
    return true
  }

  async function doFlush(): Promise<void> {
    const generation = sessionGeneration
    while (timer === null) {
      settleQuietKeys()
      const batch = collectDiff()
      if (batch.keys.length === 0) {
        break
      }
      // Captured before the await. A star-then-unstar during the request
      // bumps the generation, and the baseline has to become this sent
      // snapshot — leaving it at the pre-burst tags makes the follow-up
      // look like it has no diff, so the key stays pending forever.
      const sentByKey = new Map<string, RoomTag[]>()
      const gens = new Map<string, number>()
      for (const key of batch.keys) {
        sentByKey.set(key, cloneTags(tagsOf(key)) ?? [])
        const tagGeneration = generationByKey.get(key)
        if (tagGeneration !== undefined) {
          gens.set(key, tagGeneration)
        }
      }
      let ok: boolean
      try {
        ok = await send(batch)
      } catch {
        ok = false
      }
      if (generation !== sessionGeneration) {
        return
      }
      if (!ok) {
        skipPasses = 1
        error.value = 'Could not update favorites'
        for (const [key, tagGeneration] of gens) {
          restoreKey(key, tagGeneration)
        }
        dirtyRefresh = true
        if (migrationState.peek() === 'inflight') {
          // Arm the timer before `pending`, so the effect this write wakes
          // sees the timer and does not PUT again on the rooms signal.
          scheduleMigrationRetry()
          migrationState.value = 'pending'
        }
        if (pendingKeys.size > 0 && timer === null) {
          continue
        }
        break
      }
      for (const [key, tagGeneration] of gens) {
        if (generationByKey.get(key) !== tagGeneration) {
          const sent = sentByKey.get(key)
          if (sent !== undefined) {
            baseline.set(key, sent)
          }
          continue
        }
        // Forget the snapshot. The next edit has to read the live row, so a
        // refresh that removed the favourite is not still "order 0.5".
        baseline.delete(key)
        pendingKeys.delete(key)
        generationByKey.delete(key)
        if (localDrop.delete(key)) {
          removePinned([key])
        }
      }
    }
    if (
      generation === sessionGeneration &&
      dirtyRefresh &&
      pendingKeys.size === 0 &&
      timer === null
    ) {
      dirtyRefresh = false
      void rooms.refresh()
    }
  }

  function flush(): Promise<void> {
    if (flushWait !== null) {
      return flushWait
    }
    const run = doFlush().finally(() => {
      if (flushWait === run) {
        flushWait = null
      }
    })
    flushWait = run
    return run
  }

  function schedule(immediate: boolean): void {
    if (timer !== null) {
      clearTimeout(timer)
      timer = null
    }
    if (immediate) {
      void flush()
      return
    }
    timer = setTimeout(() => {
      timer = null
      void flush()
    }, KEYBOARD_REORDER_DEBOUNCE_MS)
  }

  function pin(key: string): boolean {
    captureIfReady()
    const assignments = pinToTopAssignments(rooms.peekRooms(), key)
    let wrote = false
    let wroteTarget = false
    for (const assignment of assignments) {
      if (
        writeTags(
          assignment.key,
          withFavourite(tagsOf(assignment.key), assignment.order),
        )
      ) {
        wrote = true
        if (assignment.key === key) {
          wroteTarget = true
        }
      }
    }
    // A room that is not in the list produces an assignment `writeTags`
    // drops. Rebalancing a neighbour still has to flush, but the command
    // did not pin the room it named.
    if (!wrote) {
      return false
    }
    if (wroteTarget && settings.pinnedRooms.peek().includes(key)) {
      localDrop.add(key)
    }
    schedule(true)
    return wroteTarget
  }

  function unpin(key: string): void {
    captureIfReady()
    const tags = tagsOf(key)
    if (!isFavourite({ tags: tags ?? [] })) {
      if (settings.pinnedRooms.peek().includes(key)) {
        removePinned([key])
      }
      return
    }
    if (!writeTags(key, withoutFavourite(tags))) {
      return
    }
    if (settings.pinnedRooms.peek().includes(key)) {
      localDrop.add(key)
    }
    schedule(true)
  }

  function move(
    key: string,
    toIndex: number,
    visibleKeys: readonly string[],
    favouriteCount: number,
    options?: { debounce?: boolean },
  ): void {
    captureIfReady()
    const plan = planFavouriteMove(key, toIndex, visibleKeys, favouriteCount, {
      orderOf,
      isFavourite: keyIsFavourite,
    })
    if (plan.kind === 'noop') {
      return
    }
    if (plan.kind === 'drop-local') {
      removePinned([plan.key])
      return
    }
    if (plan.kind === 'unpin') {
      unpin(plan.key)
      return
    }
    let wrote = false
    for (const assignment of plan.assignments) {
      if (
        writeTags(
          assignment.key,
          withFavourite(tagsOf(assignment.key), assignment.order),
        )
      ) {
        wrote = true
      }
    }
    if (!wrote) {
      return
    }
    if (settings.pinnedRooms.peek().includes(key)) {
      localDrop.add(key)
    }
    schedule(options?.debounce !== true)
  }

  async function runMigration(): Promise<void> {
    if (migrationState.peek() !== 'pending' || !armed.peek()) {
      return
    }
    if (pendingKeys.size > 0 || flushWait !== null || timer !== null) {
      return
    }
    const pins = settings.pinnedRooms.peek()
    if (pins.length === 0) {
      migrationState.value = 'done'
      return
    }
    const generation = sessionGeneration
    if (
      !rooms.confirmed.peek() ||
      rooms.stale.peek() ||
      rooms.loading.peek() ||
      accounts.loading.peek()
    ) {
      return
    }
    if (
      accounts.error.peek() !== null &&
      accounts.accounts.peek().length === 0
    ) {
      return
    }
    captureIfReady()
    const listed = new Map(
      accounts.accounts.peek().map((account) => [account.account_id, account]),
    )
    const accountsLoaded = accounts.accounts.peek().length > 0
    const present = new Set(rooms.peekRooms().map((room) => roomKey(room)))
    const accountsInRooms = new Set(
      rooms.peekRooms().map((room) => room.account_id),
    )
    const serverWins = serverFavouriteAccounts ?? new Set<string>()
    const upload: string[] = []
    const discard: string[] = []
    for (const key of pins) {
      const parts = splitRoomKey(key)
      if (parts === null) {
        discard.push(key)
        continue
      }
      const account = listed.get(parts.accountId)
      if (
        (account !== undefined && account.state !== 'active') ||
        (accountsLoaded && account === undefined) ||
        serverWins.has(parts.accountId)
      ) {
        discard.push(key)
      } else if (present.has(key)) {
        upload.push(key)
      } else if (accountsInRooms.has(parts.accountId)) {
        discard.push(key)
      }
    }
    if (discard.length > 0) {
      removePinned(discard)
    }
    if (upload.length === 0) {
      if (settings.pinnedRooms.peek().length === 0) {
        migrationState.value = 'done'
      }
      return
    }
    const assignments = migrationAssignments(upload)
    let wrote = false
    for (const assignment of assignments) {
      if (
        writeTags(
          assignment.key,
          withFavourite(tagsOf(assignment.key), assignment.order),
        )
      ) {
        wrote = true
        localDrop.add(assignment.key)
      }
    }
    if (!wrote) {
      return
    }
    migrationState.value = 'inflight'
    await flush()
    if (
      generation !== sessionGeneration ||
      migrationState.peek() !== 'inflight'
    ) {
      return
    }
    clearMigrationRetry()
    migrationState.value =
      settings.pinnedRooms.peek().length === 0 ? 'done' : 'pending'
  }

  effect(() => {
    // A refresh replaces tags without going through writeTags. A baseline
    // left from the last successful flush would still describe that old
    // favourite, and a later pin at the same order would send nothing.
    // Pending keys keep the snapshot the flush is diffing against.
    void rooms.rooms.value
    for (const key of [...baseline.keys()]) {
      if (!pendingKeys.has(key)) {
        baseline.delete(key)
      }
    }
  })

  effect(() => {
    if (!armed.value) {
      return
    }
    void rooms.rooms.value
    void rooms.confirmed.value
    void rooms.stale.value
    void rooms.loading.value
    void accounts.accounts.value
    void accounts.loading.value
    void accounts.error.value
    void settings.pinnedRooms.value
    // Subscribe even when a retry is already waiting, so a list change does
    // not have to be what wakes the next attempt.
    void migrationState.value
    if (retryTimer !== null) {
      return
    }
    if (migrationState.value !== 'pending') {
      return
    }
    if (skipPasses > 0) {
      skipPasses -= 1
      return
    }
    void runMigration()
  })

  let seenReconnects = live.reconnects.peek()
  effect(() => {
    const reconnects = live.reconnects.value
    if (!armed.value) {
      seenReconnects = reconnects
      return
    }
    if (reconnects === seenReconnects) {
      return
    }
    seenReconnects = reconnects
    if (reconnects === 0) {
      return
    }
    void rooms.refresh()
  })

  live.subscribe((frame) => {
    const change = accountDataChange(frame)
    if (change === null) {
      return
    }
    if (change.eventType === 'm.tag' && change.roomId !== null) {
      const key = `${frame.accountId}/${change.roomId}`
      if (pendingKeys.has(key)) {
        dirtyRefresh = true
        return
      }
      rooms.applyAccountData(frame.accountId, change)
      baseline.set(key, cloneTags(tagsOf(key)))
      return
    }
    if (change.eventType === 'm.direct') {
      rooms.applyAccountData(frame.accountId, change)
    }
  })

  return {
    reordering,
    error,
    pin,
    unpin,
    move,
    start() {
      if (armed.peek()) {
        return
      }
      migrationState.value = 'pending'
      armed.value = true
    },
    resetSession() {
      sessionGeneration += 1
      if (timer !== null) {
        clearTimeout(timer)
        timer = null
      }
      pendingKeys.clear()
      baseline.clear()
      generationByKey.clear()
      localDrop.clear()
      clientWrittenAccounts.clear()
      serverFavouriteAccounts = null
      captured = false
      dirtyRefresh = false
      skipPasses = 0
      clearMigrationRetry()
      flushWait = null
      if (reordering.peek()) {
        reordering.value = false
      }
      if (error.peek() !== null) {
        error.value = null
      }
      if (armed.peek()) {
        armed.value = false
      }
      if (migrationState.peek() !== 'idle') {
        migrationState.value = 'idle'
      }
    },
  }
}
