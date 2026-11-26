import {
  batch,
  computed,
  effect,
  signal,
  untracked,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import { roomDetailsChange, timelineEvent } from '../api/frames'
import type { ApiClient } from '../api/client'
import type { AccountsStore } from './accounts'
import type { LiveConnection } from './live-connection'
import { roomKey, roomTitle, type RoomDto } from './room-list'
import type { RoomsStore } from './rooms'
import {
  projectSpaces,
  reachableKeys,
  type DiscoveredMembership,
  type SpaceGraph,
} from './space-graph'
import {
  readDiscoveredMemberships,
  readSpaceGraph,
  spaceLoadError,
} from './space-graph-api'

export interface SpacesStore {
  selected: Signal<string | null>
  /** Browser-local account narrowing shared by the rail and the room list. */
  accountFilter: Signal<string>
  effectiveAccountFilter: ReadonlySignal<string>
  visible: ReadonlySignal<readonly RoomDto[]>
  subspaces: ReadonlySignal<ReadonlyMap<string, readonly string[]>>
  nested: ReadonlySignal<ReadonlySet<string>>
  expanded: Signal<ReadonlySet<string>>
  /** Total unread notifications across distinct reachable joined chat rooms. */
  unread: ReadonlySignal<ReadonlyMap<string, number>>
  /** Joined chat room keys in the selected space, once its graph is known. */
  selectedRooms: ReadonlySignal<ReadonlySet<string> | null>
  parents: ReadonlySignal<
    ReadonlyMap<string, ReadonlyMap<string, readonly string[]>>
  >
  errors: ReadonlySignal<ReadonlyMap<string, string>>
  loading: ReadonlySignal<ReadonlySet<string>>
  reordering: Signal<boolean>
  refresh(space: Pick<RoomDto, 'account_id'>): void
  resetSession(): void
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

export interface SpacePickerEntry {
  space: RoomDto
  path: string
  depth: number
  expandable: boolean
}

/** Picker display and keyboard cycling use the same expanded branch order. */
export function spacePickerEntries(
  spaces: SpacesStore,
  order: readonly string[],
  titles: ReadonlyMap<string, string>,
): SpacePickerEntry[] {
  const visible = spaces.visible.value
  const byKey = new Map(visible.map((space) => [roomKey(space), space]))
  const roots = orderedSpaces(
    visible.filter((space) => !spaces.nested.value.has(roomKey(space))),
    order,
    titles,
  )
  const subspaces = spaces.subspaces.value
  const expanded = spaces.expanded.value
  const pending = roots.reverse().map((space) => ({
    space,
    path: JSON.stringify(roomKey(space)),
    depth: 0,
    ancestors: new Set<string>(),
  }))
  const entries: SpacePickerEntry[] = []
  while (pending.length > 0) {
    const { space, path, depth, ancestors } = pending.pop()!
    const key = roomKey(space)
    const children = (subspaces.get(key) ?? []).filter(
      (child) => !ancestors.has(child),
    )
    entries.push({ space, path, depth, expandable: children.length > 0 })
    if (!expanded.has(path)) continue
    const nextAncestors = new Set(ancestors).add(key)
    for (let index = children.length - 1; index >= 0; index -= 1) {
      const child = children[index]
      const childSpace = byKey.get(child)
      if (childSpace !== undefined) {
        pending.push({
          space: childSpace,
          path: `${path}/${JSON.stringify(child)}`,
          depth: depth + 1,
          ancestors: nextAncestors,
        })
      }
    }
  }
  return entries
}

type AccountRead = {
  spaces: ReadonlyMap<string, string>
  abort: AbortController | null
}

const MAX_ACCOUNT_READS = 3
const MAX_TIMER_DELAY = 2_147_483_647

export function createSpacesStore(
  api: ApiClient,
  rooms: RoomsStore,
  live: LiveConnection,
  accountFilter: Signal<string> = signal('all'),
  accounts?: Pick<AccountsStore, 'accounts' | 'loading' | 'error'>,
): SpacesStore {
  const selected = signal<string | null>(null)
  const expanded = signal<ReadonlySet<string>>(new Set())
  const reordering = signal(false)
  const effectiveAccountFilter = computed(() => {
    const saved = accountFilter.value
    if (accounts === undefined || saved === 'all') return saved
    return !accounts.loading.value &&
      accounts.error.value === null &&
      accounts.accounts.value.some(
        (account) => account.account_id === saved && account.state === 'active',
      )
      ? saved
      : 'all'
  })

  const graphs = signal<ReadonlyMap<string, SpaceGraph>>(new Map())
  const discovered = signal<
    ReadonlyMap<string, readonly DiscoveredMembership[]>
  >(new Map())
  const accountErrors = signal<ReadonlyMap<string, string>>(new Map())
  const loading = signal<ReadonlySet<string>>(new Set())
  const reads = new Map<string, AccountRead>()
  const queued = new Set<string>()
  let activeReads = 0

  // Room DTOs also change on ordinary messages. Only membership and room
  // type affect the graph, so leave its projection intact on those writes.
  const catalog = signal<ReadonlyMap<string, RoomDto>>(new Map())
  effect(() => {
    const current = rooms.rooms.value
    const previous = catalog.peek()
    if (
      current.length !== previous.size ||
      current.some((room) => {
        const entry = previous.get(roomKey(room))
        return entry === undefined || entry.room_type !== room.room_type
      })
    ) {
      catalog.value = new Map(current.map((room) => [roomKey(room), room]))
    }
  })

  const now = signal(Date.now())
  effect(() => {
    const time = now.value
    let nextExpiry = Infinity
    for (const memberships of discovered.value.values()) {
      for (const { expires_at } of memberships) {
        if (expires_at > time) nextExpiry = Math.min(nextExpiry, expires_at)
      }
    }
    if (nextExpiry === Infinity) return
    const timer = setTimeout(
      () => {
        now.value = Date.now()
      },
      Math.min(MAX_TIMER_DELAY, Math.max(0, nextExpiry - Date.now())),
    )
    return () => clearTimeout(timer)
  })

  const projection = computed(() =>
    projectSpaces(catalog.value, graphs.value, discovered.value, now.value),
  )
  const ready = (key: string, accountId: string) => {
    const graph = graphs.value.get(accountId)
    return (
      graph !== undefined && (!('legacy' in graph) || graph.legacy.has(key))
    )
  }
  const errors = computed(() => {
    const next = new Map<string, string>()
    for (const [key, room] of catalog.value) {
      if (room.room_type !== 'm.space') continue
      const graph = graphs.value.get(room.account_id)
      const error =
        accountErrors.value.get(room.account_id) ??
        (graph !== undefined && 'legacy' in graph
          ? graph.errors.get(key)
          : undefined)
      if (error !== undefined) next.set(key, error)
    }
    return next
  })
  const visible = computed(() => {
    const account = effectiveAccountFilter.value
    const uncertain = rooms.stale.value || rooms.error.value !== null
    const { members, subspaces, nested } = projection.value
    return rooms.rooms.value.filter((room) => {
      if (room.room_type !== 'm.space') return false
      if (account !== 'all' && room.account_id !== account) return false
      const key = roomKey(room)
      return (
        uncertain ||
        !ready(key, room.account_id) ||
        errors.value.has(key) ||
        members.has(key) ||
        subspaces.has(key) ||
        nested.has(key)
      )
    })
  })
  const selectedRooms = computed(() => {
    const key = selected.value
    const space = key === null ? undefined : catalog.value.get(key)
    if (key === null || space === undefined || !ready(key, space.account_id)) {
      return null
    }
    const joined = reachableKeys(key, projection.value.members)
    for (const member of joined) {
      if (catalog.value.get(member)?.room_type === 'm.space')
        joined.delete(member)
    }
    return joined
  })
  const unread = computed(() => {
    const totals = new Map<string, number>()
    const { ancestors } = projection.value
    for (const key of ancestors.keys()) {
      if (catalog.value.get(key)?.room_type === 'm.space') continue
      const count = rooms.unreadCount(key)
      if (count === 0) continue
      for (const space of reachableKeys(key, ancestors)) {
        totals.set(space, (totals.get(space) ?? 0) + count)
      }
    }
    return totals
  })

  function runQueued(): void {
    for (const accountId of queued) {
      if (activeReads >= MAX_ACCOUNT_READS) return
      const read = reads.get(accountId)
      if (read === undefined || read.abort !== null) continue
      queued.delete(accountId)
      const abort = new AbortController()
      read.abort = abort
      activeReads += 1
      void readAccount(accountId, read, abort)
    }
  }

  async function readAccount(
    accountId: string,
    read: AccountRead,
    abort: AbortController,
  ): Promise<void> {
    const requestedSpaces = read.spaces
    try {
      const [graph, memberships] = await Promise.allSettled([
        readSpaceGraph(api, accountId, requestedSpaces, abort.signal),
        readDiscoveredMemberships(api, accountId, abort.signal),
      ])
      if (abort.signal.aborted) return
      batch(() => {
        const nextErrors = new Map(accountErrors.peek())
        if (graph.status === 'fulfilled') {
          let nextGraph = graph.value
          if ('legacy' in nextGraph) {
            if (requestedSpaces !== read.spaces) queued.add(accountId)
            const previous = graphs.peek().get(accountId)
            if (previous !== undefined && 'legacy' in previous) {
              const legacy = new Map(nextGraph.legacy)
              for (const key of nextGraph.errors.keys()) {
                const cached = previous.legacy.get(key)
                if (cached !== undefined) legacy.set(key, cached)
              }
              nextGraph = { ...nextGraph, legacy }
            }
          }
          graphs.value = new Map(graphs.peek()).set(accountId, nextGraph)
          nextErrors.delete(accountId)
        } else {
          nextErrors.set(accountId, spaceLoadError(graph.reason))
        }
        if (memberships.status === 'fulfilled') {
          now.value = Date.now()
          discovered.value = new Map(discovered.peek()).set(
            accountId,
            memberships.value,
          )
        } else if (!nextErrors.has(accountId)) {
          nextErrors.set(accountId, spaceLoadError(memberships.reason))
        }
        accountErrors.value = nextErrors
      })
    } finally {
      activeReads -= 1
      read.abort = null
      if (reads.get(accountId) === read && !queued.has(accountId)) {
        const next = new Set(loading.peek())
        next.delete(accountId)
        loading.value = next
      }
      runQueued()
    }
  }

  function refreshAccount(accountId: string): void {
    if (!reads.has(accountId)) return
    queued.add(accountId)
    if (!loading.peek().has(accountId)) {
      loading.value = new Set(loading.peek()).add(accountId)
    }
    runQueued()
  }

  function removeAccount(accountId: string): void {
    reads.get(accountId)?.abort?.abort()
    reads.delete(accountId)
    queued.delete(accountId)
    const nextLoading = new Set(loading.peek())
    const nextGraphs = new Map(graphs.peek())
    const nextDiscovered = new Map(discovered.peek())
    const nextErrors = new Map(accountErrors.peek())
    if (nextLoading.delete(accountId)) loading.value = nextLoading
    if (nextGraphs.delete(accountId)) graphs.value = nextGraphs
    if (nextDiscovered.delete(accountId)) discovered.value = nextDiscovered
    if (nextErrors.delete(accountId)) accountErrors.value = nextErrors
  }

  effect(() => {
    const byAccount = new Map<string, Map<string, string>>()
    for (const [key, room] of catalog.value) {
      if (room.room_type !== 'm.space') continue
      const spaces = byAccount.get(room.account_id) ?? new Map()
      spaces.set(key, room.room_id)
      byAccount.set(room.account_id, spaces)
    }
    for (const accountId of reads.keys()) {
      if (!byAccount.has(accountId)) removeAccount(accountId)
    }
    for (const [accountId, spaces] of byAccount) {
      const read = reads.get(accountId)
      if (read === undefined) {
        reads.set(accountId, { spaces, abort: null })
        refreshAccount(accountId)
      } else if (
        read.spaces.size !== spaces.size ||
        [...spaces.keys()].some((key) => !read.spaces.has(key))
      ) {
        read.spaces = spaces
        // The account-wide graph already includes links for a newly listed
        // space. Only the legacy read needs to fetch it individually.
        const graph = graphs.peek().get(accountId)
        if (graph !== undefined && 'legacy' in graph) refreshAccount(accountId)
      }
    }
  })

  effect(() => {
    const key = selected.value
    if (
      key !== null &&
      !visible.value.some((space) => roomKey(space) === key)
    ) {
      selected.value = null
    }
  })

  live.subscribe((frame) => {
    const change = roomDetailsChange(frame)
    if (
      change?.sources.has('space_graph') ||
      change?.sources.has('space_browse')
    ) {
      refreshAccount(frame.accountId)
      return
    }
    // Servers predating the graph only announce child state in the timeline.
    const graph = graphs.peek().get(frame.accountId)
    if (graph !== undefined && 'links' in graph) return
    const event = timelineEvent(frame)
    if (
      event?.type === 'm.space.child' &&
      reads.get(frame.accountId)?.spaces.has(roomKey(event))
    ) {
      refreshAccount(frame.accountId)
    }
  })

  let lastReconnects = live.reconnects.peek()
  effect(() => {
    const reconnects = live.reconnects.value
    if (reconnects === lastReconnects) return
    lastReconnects = reconnects
    untracked(() => {
      for (const accountId of reads.keys()) refreshAccount(accountId)
    })
  })

  return {
    selected,
    accountFilter,
    effectiveAccountFilter,
    visible,
    subspaces: computed(() => projection.value.subspaces),
    nested: computed(() => projection.value.nested),
    expanded,
    unread,
    selectedRooms,
    parents: computed(() => projection.value.parents),
    errors,
    loading,
    reordering,
    refresh: ({ account_id }) => refreshAccount(account_id),
    resetSession: () => {
      for (const accountId of reads.keys()) removeAccount(accountId)
      if (expanded.peek().size > 0) expanded.value = new Set()
      if (selected.peek() !== null) selected.value = null
    },
  }
}
