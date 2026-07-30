import {
  effect,
  signal,
  untracked,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import type { components } from '../api/schema'
import { timelineEvent } from '../api/frames'
import { apiErrorMessage, type ApiClient } from '../api/client'
import type { LiveConnection } from './live-connection'
import { roomKey, roomTitle, type RoomDto } from './room-list'
import type { RoomsStore } from './rooms'

export type SpaceChildDto = components['schemas']['SpaceChildDto']

export interface SpacesStore {
  selected: Signal<string | null>
  children: ReadonlySignal<ReadonlyMap<string, readonly SpaceChildDto[]>>
  loading: ReadonlySignal<ReadonlySet<string>>
  errors: ReadonlySignal<ReadonlyMap<string, string>>
  /**
   * Whether the picker shows its per-space move controls. Session-only and off
   * by default: the rail is kept as narrow and short as possible, so this
   * chrome appears only when the user asks for it (`KEYS.reorderSpaces`).
   */
  reordering: Signal<boolean>
  refresh(space: Pick<RoomDto, 'account_id' | 'room_id'>): void
}

/** Order the picker exactly as the user sees it: saved ranks, then title. */
export function orderedSpaces(
  spaces: readonly RoomDto[],
  order: readonly string[],
  titles: ReadonlyMap<string, string>,
): RoomDto[] {
  const rank = new Map(order.map((key, index) => [key, index]))
  return [...spaces].sort((left, right) => {
    const leftRank = rank.get(roomKey(left))
    const rightRank = rank.get(roomKey(right))
    if (leftRank !== undefined || rightRank !== undefined) {
      if (leftRank === undefined) return 1
      if (rightRank === undefined) return -1
      return leftRank - rightRank
    }
    return roomTitle(left, titles).localeCompare(roomTitle(right, titles))
  })
}

/**
 * Joined spaces are ordinary RoomDto entries; this store adds their ordered
 * `m.space.child` projection without making the room list understand HTTP.
 */
export function createSpacesStore(
  api: ApiClient,
  rooms: RoomsStore,
  live: LiveConnection,
): SpacesStore {
  const selected = signal<string | null>(null)
  const reordering = signal(false)
  const children = signal<ReadonlyMap<string, readonly SpaceChildDto[]>>(
    new Map(),
  )
  const loading = signal<ReadonlySet<string>>(new Set())
  const errors = signal<ReadonlyMap<string, string>>(new Map())
  const requested = new Set<string>()
  const queued = new Set<string>()
  const dirty = new Set<string>()
  const queue: Array<Pick<RoomDto, 'account_id' | 'room_id'>> = []
  let activeRequests = 0

  const joinedSpaces = () =>
    rooms.rooms.value.filter((room) => room.room_type === 'm.space')

  const runNext = () => {
    if (activeRequests >= 6) return
    const space = queue.shift()
    if (space === undefined) return
    activeRequests += 1
    const key = roomKey(space)
    void api
      .GET('/v1/accounts/{account_id}/rooms/{room_id}/space/children', {
        params: {
          path: { account_id: space.account_id, room_id: space.room_id },
        },
      })
      .then(({ data, error }) => {
        const nextErrors = new Map(errors.value)
        if (data === undefined) {
          nextErrors.set(key, `Could not load space: ${apiErrorMessage(error)}`)
        } else {
          children.value = new Map(children.value).set(key, data.data)
          nextErrors.delete(key)
        }
        errors.value = nextErrors
      })
      .catch(() => {
        errors.value = new Map(errors.value).set(key, 'Could not load space.')
      })
      .finally(() => {
        queued.delete(key)
        activeRequests -= 1
        // A refresh that arrived mid-flight (an `m.space.child` frame, say) was
        // coalesced into this request, which had already been sent and cannot
        // reflect it. Re-queue rather than drop it, or the child list stays
        // stale until the next reconnect.
        if (dirty.delete(key)) {
          refresh(space)
        } else {
          const next = new Set(loading.value)
          next.delete(key)
          loading.value = next
        }
        runNext()
      })
    runNext()
  }

  const refresh = (space: Pick<RoomDto, 'account_id' | 'room_id'>) => {
    const key = roomKey(space)
    if (queued.has(key)) {
      // Already queued: if it is still waiting in `queue` it will pick up the
      // change anyway; if it is in flight, mark it for a follow-up fetch.
      if (!queue.some((pending) => roomKey(pending) === key)) dirty.add(key)
      return
    }
    queued.add(key)
    queue.push(space)
    loading.value = new Set(loading.value).add(key)
    runNext()
  }

  effect(() => {
    const spaces = joinedSpaces()
    const present = new Set(spaces.map(roomKey))
    for (const space of spaces) {
      const key = roomKey(space)
      if (!requested.has(key)) {
        requested.add(key)
        refresh(space)
      }
    }
    if (selected.value !== null && !present.has(selected.value))
      selected.value = null
  })

  live.subscribe((frame) => {
    const event = timelineEvent(frame)
    if (event === null || event.type !== 'm.space.child') return
    const space = joinedSpaces().find(
      (candidate) =>
        candidate.account_id === event.account_id &&
        candidate.room_id === event.room_id,
    )
    if (space !== undefined) refresh(space)
  })
  // A reconnect means the socket missed frames, so every space is refetched.
  // `joinedSpaces()` reads `rooms.rooms`, which changes on every incoming
  // timeline event; reading it inside the effect body would subscribe this
  // effect to it and turn each message into a refetch of every space. Track the
  // reconnect count explicitly and read the room list untracked.
  let lastReconnects = live.reconnects.peek()
  effect(() => {
    const reconnects = live.reconnects.value
    if (reconnects === lastReconnects) return
    lastReconnects = reconnects
    for (const space of untracked(joinedSpaces)) refresh(space)
  })

  return { selected, children, loading, errors, reordering, refresh }
}
