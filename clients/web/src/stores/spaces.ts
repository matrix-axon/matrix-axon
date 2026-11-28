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
  type ParentRoutes,
  type SpaceChildDto,
  type SpaceGraphLink,
} from './space-graph'
import {
  readDiscoveredMemberships,
  readLegacyChildren,
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
  expanded: ReadonlySignal<ReadonlySet<string>>
  setExpanded(path: string, isExpanded: boolean): void
  /** Total unread notifications across distinct reachable joined chat rooms. */
  unread: ReadonlySignal<ReadonlyMap<string, number>>
  /** Joined chat room keys in the selected space, once its graph is known. */
  selectedRooms: ReadonlySignal<ReadonlySet<string> | null>
  parents: ReadonlySignal<
    ReadonlyMap<string, ReadonlyMap<string, ParentRoutes>>
  >
  errors: ReadonlySignal<ReadonlyMap<string, string>>
  loading: ReadonlySignal<ReadonlySet<string>>
  reordering: Signal<boolean>
  refresh(space: Pick<RoomDto, 'account_id' | 'room_id'>): void
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
  parentPath: string | null
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
    parentPath: null as string | null,
    depth: 0,
    ancestors: new Set<string>(),
  }))
  const entries: SpacePickerEntry[] = []
  while (pending.length > 0) {
    const { space, path, parentPath, depth, ancestors } = pending.pop()!
    const key = roomKey(space)
    const children = (subspaces.get(key) ?? []).filter(
      (child) => !ancestors.has(child),
    )
    entries.push({
      space,
      path,
      parentPath,
      depth,
      expandable: children.length > 0,
    })
    if (!expanded.has(path)) continue
    const nextAncestors = new Set(ancestors).add(key)
    for (let index = children.length - 1; index >= 0; index -= 1) {
      const child = children[index]
      const childSpace = byKey.get(child)
      if (childSpace !== undefined) {
        pending.push({
          space: childSpace,
          path: `${path}/${JSON.stringify(child)}`,
          parentPath: path,
          depth: depth + 1,
          ancestors: nextAncestors,
        })
      }
    }
  }
  return entries
}

/** Step from the selected row, or from its position below a collapsed ancestor. */
export function adjacentSpace(
  spaces: SpacesStore,
  order: readonly string[],
  titles: ReadonlyMap<string, string>,
  direction: -1 | 1,
): string | undefined {
  const entries = spacePickerEntries(spaces, order, titles)
  const ordered = [...new Set(entries.map(({ space }) => roomKey(space)))]
  if (ordered.length === 0) return undefined
  const current = spaces.selected.value
  const index = current === null ? -1 : ordered.indexOf(current)
  if (index !== -1) {
    return ordered[(index + direction + ordered.length) % ordered.length]
  }
  if (current !== null) {
    const byPath = new Map(entries.map((entry) => [entry.path, entry]))
    const children = spaces.subspaces.value
    const expanded = spaces.expanded.value
    const branch = entries.find((entry) => {
      if (!entry.expandable || expanded.has(entry.path)) return false
      const blocked = new Set<string>()
      for (let path = entry.parentPath; path !== null;) {
        const parent = byPath.get(path)!
        blocked.add(roomKey(parent.space))
        path = parent.parentPath
      }
      const reachable = new Set([roomKey(entry.space)])
      for (const key of reachable) {
        if (key === current) return true
        for (const child of children.get(key) ?? []) {
          if (!blocked.has(child)) reachable.add(child)
        }
      }
      return false
    })
    if (branch !== undefined) {
      const anchor = ordered.indexOf(roomKey(branch.space))
      // A collapsed branch's descendants would follow its own row.
      return ordered[(anchor + (direction === 1 ? 1 : 0)) % ordered.length]
    }
  }
  return ordered[direction === 1 ? 0 : ordered.length - 1]
}

type SpaceRead =
  | { kind: 'graph' | 'discovered'; accountId: string }
  | { kind: 'legacy'; accountId: string; spaceKey: string; roomId: string }

const MAX_SPACE_READS = 6
const MAX_TIMER_DELAY = 2_147_483_647

export function createSpacesStore(
  api: ApiClient,
  rooms: RoomsStore,
  live: LiveConnection,
  accountFilter: Signal<string> = signal('all'),
  accounts?: Pick<AccountsStore, 'accounts' | 'loading' | 'error'>,
  expandedPaths: Signal<readonly string[]> = signal([]),
): SpacesStore {
  const selected = signal<string | null>(null)
  const expanded = computed(() => new Set(expandedPaths.value))
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

  // A null graph records a server that requires per-space legacy reads.
  const graphs = signal<ReadonlyMap<string, readonly SpaceGraphLink[] | null>>(
    new Map(),
  )
  const legacy = signal<ReadonlyMap<string, readonly SpaceChildDto[]>>(
    new Map(),
  )
  const legacyErrors = signal<ReadonlyMap<string, string>>(new Map())
  const discovered = signal<
    ReadonlyMap<string, readonly DiscoveredMembership[]>
  >(new Map())
  const accountErrors = signal<ReadonlyMap<string, string>>(new Map())
  const loading = signal<ReadonlySet<string>>(new Set())
  const knownSpaces = new Map<string, ReadonlyMap<string, string>>()
  const queued = new Map<string, SpaceRead>()
  const running = new Map<string, AbortController>()

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

  effect(() => {
    // The room store clears confirmation before dropping rows on sign-out.
    // A cached or failed read cannot establish that a space was left.
    if (
      !rooms.confirmed.value ||
      rooms.stale.value ||
      rooms.error.value !== null
    )
      return
    const joined = catalog.value
    const saved = expandedPaths.peek()
    if (saved.length === 0) return
    const spaceKeys = new Set<string>()
    for (const [key, room] of joined) {
      if (room.room_type === 'm.space') spaceKeys.add(JSON.stringify(key))
    }
    const retained = saved.filter((path) => {
      // Picker paths join JSON-quoted keys with "/", which also occurs inside
      // room keys. Match the quoted segments before checking membership.
      const keys = path.match(/"(?:[^"\\]|\\.)*"/g)
      return (
        keys !== null &&
        keys.join('/') === path &&
        keys.every((key) => spaceKeys.has(key))
      )
    })
    if (retained.length !== saved.length) expandedPaths.value = retained
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
    projectSpaces(
      catalog.value,
      graphs.value,
      legacy.value,
      discovered.value,
      now.value,
    ),
  )
  const ready = (key: string, accountId: string) => {
    const graph = graphs.value.get(accountId)
    return graph === null ? legacy.value.has(key) : graph !== undefined
  }
  const errors = computed(() => {
    const next = new Map<string, string>()
    for (const [key, room] of catalog.value) {
      if (room.room_type !== 'm.space') continue
      const error =
        legacyErrors.value.get(key) ?? accountErrors.value.get(room.account_id)
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
        (graphs.value.get(room.account_id) !== null &&
          !discovered.value.has(room.account_id)) ||
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
    for (const [id, read] of queued) {
      if (running.size >= MAX_SPACE_READS) return
      if (running.has(id)) continue
      queued.delete(id)
      const abort = new AbortController()
      running.set(id, abort)
      void runRead(id, read, abort.signal)
    }
  }

  async function runRead(id: string, read: SpaceRead, signal: AbortSignal) {
    const { accountId } = read
    try {
      switch (read.kind) {
        case 'graph': {
          const graph = await readSpaceGraph(api, accountId, signal)
          if (signal.aborted) break
          batch(() => {
            const previous = graphs.peek().get(accountId)
            graphs.value = new Map(graphs.peek()).set(accountId, graph)
            deleteEntry(accountErrors, accountId)
            if (graph === null) refreshLegacyAccount(accountId)
            else if (previous === null) {
              for (const key of knownSpaces.get(accountId)?.keys() ?? [])
                dropLegacy(key)
            }
          })
          break
        }
        case 'discovered': {
          const memberships = await readDiscoveredMemberships(
            api,
            accountId,
            signal,
          )
          if (signal.aborted) break
          batch(() => {
            now.value = Date.now()
            discovered.value = new Map(discovered.peek()).set(
              accountId,
              memberships,
            )
          })
          break
        }
        case 'legacy': {
          const children = await readLegacyChildren(
            api,
            accountId,
            read.roomId,
            signal,
          )
          if (signal.aborted) break
          batch(() => {
            legacy.value = new Map(legacy.peek()).set(read.spaceKey, children)
            deleteEntry(legacyErrors, read.spaceKey)
          })
          break
        }
      }
    } catch (error) {
      if (!signal.aborted) {
        if (read.kind === 'graph') {
          if (graphs.peek().get(accountId) === null)
            refreshLegacyAccount(accountId)
          else
            accountErrors.value = new Map(accountErrors.peek()).set(
              accountId,
              spaceLoadError(error),
            )
        } else if (read.kind === 'legacy') {
          legacyErrors.value = new Map(legacyErrors.peek()).set(
            read.spaceKey,
            spaceLoadError(error),
          )
        } else if (!discovered.peek().has(accountId)) {
          // Settle the first read even on failure so the graph can confirm
          // emptiness. A failed refresh keeps memberships until they expire.
          discovered.value = new Map(discovered.peek()).set(accountId, [])
        }
      }
    } finally {
      running.delete(id)
      if (!queued.has(id)) stopLoading(id)
      runQueued()
    }
  }

  function queueRead(read: SpaceRead) {
    const id =
      read.kind === 'legacy'
        ? `legacy/${read.spaceKey}`
        : `${read.kind}/${read.accountId}`
    queued.set(id, read)
    if (!loading.peek().has(id)) loading.value = new Set(loading.peek()).add(id)
    runQueued()
  }

  function stopLoading(id: string) {
    if (!loading.peek().has(id)) return
    const next = new Set(loading.peek())
    next.delete(id)
    loading.value = next
  }

  function cancelRead(id: string) {
    queued.delete(id)
    running.get(id)?.abort()
    stopLoading(id)
  }

  function deleteEntry<T>(
    entries: Signal<ReadonlyMap<string, T>>,
    key: string,
  ) {
    if (!entries.peek().has(key)) return
    const next = new Map(entries.peek())
    next.delete(key)
    entries.value = next
  }

  function dropLegacy(key: string) {
    cancelRead(`legacy/${key}`)
    deleteEntry(legacy, key)
    deleteEntry(legacyErrors, key)
  }

  function refreshLegacyAccount(accountId: string) {
    for (const [spaceKey, roomId] of knownSpaces.get(accountId) ?? []) {
      queueRead({ kind: 'legacy', accountId, spaceKey, roomId })
    }
  }

  function refreshAccount(accountId: string) {
    queueRead({ kind: 'graph', accountId })
    queueRead({ kind: 'discovered', accountId })
  }

  function removeAccount(accountId: string) {
    for (const key of knownSpaces.get(accountId)?.keys() ?? []) dropLegacy(key)
    knownSpaces.delete(accountId)
    cancelRead(`graph/${accountId}`)
    cancelRead(`discovered/${accountId}`)
    deleteEntry(graphs, accountId)
    deleteEntry(discovered, accountId)
    deleteEntry(accountErrors, accountId)
  }

  effect(() => {
    const byAccount = new Map<string, Map<string, string>>()
    for (const [key, room] of catalog.value) {
      if (room.room_type !== 'm.space') continue
      const spaces = byAccount.get(room.account_id) ?? new Map()
      spaces.set(key, room.room_id)
      byAccount.set(room.account_id, spaces)
    }
    for (const accountId of knownSpaces.keys()) {
      if (!byAccount.has(accountId)) removeAccount(accountId)
    }
    for (const [accountId, spaces] of byAccount) {
      const previous = knownSpaces.get(accountId)
      knownSpaces.set(accountId, spaces)
      if (previous === undefined) {
        refreshAccount(accountId)
      } else if (graphs.peek().get(accountId) === null) {
        for (const key of previous.keys()) {
          if (!spaces.has(key)) dropLegacy(key)
        }
        for (const [spaceKey, roomId] of spaces) {
          if (!previous.has(spaceKey))
            queueRead({ kind: 'legacy', accountId, spaceKey, roomId })
        }
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
    const accountId = frame.accountId
    if (!knownSpaces.has(accountId)) return
    const change = roomDetailsChange(frame)
    if (change?.sources.has('space_graph')) {
      refreshAccount(accountId)
      return
    }
    if (change?.sources.has('space_browse')) {
      queueRead({ kind: 'discovered', accountId })
      return
    }
    // The older server announces child state in the room timeline.
    const event = timelineEvent(frame)
    if (
      graphs.peek().get(accountId) === null &&
      event?.type === 'm.space.child'
    ) {
      refreshLegacy(event)
    }
  })

  function refreshLegacy(space: Pick<RoomDto, 'account_id' | 'room_id'>) {
    const spaceKey = roomKey(space)
    if (knownSpaces.get(space.account_id)?.has(spaceKey)) {
      queueRead({
        kind: 'legacy',
        accountId: space.account_id,
        spaceKey,
        roomId: space.room_id,
      })
    }
  }

  let lastReconnects = live.reconnects.peek()
  effect(() => {
    const reconnects = live.reconnects.value
    if (reconnects === lastReconnects) return
    lastReconnects = reconnects
    untracked(() => {
      for (const accountId of knownSpaces.keys()) refreshAccount(accountId)
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
    setExpanded: (path, isExpanded) => {
      if (expanded.peek().has(path) === isExpanded) return
      const paths = expandedPaths.peek()
      expandedPaths.value = isExpanded
        ? [...paths, path]
        : paths.filter((candidate) => candidate !== path)
    },
    unread,
    selectedRooms,
    parents: computed(() => projection.value.parents),
    errors,
    loading,
    reordering,
    refresh: (space) => {
      if (!knownSpaces.has(space.account_id)) return
      if (graphs.peek().get(space.account_id) === null) refreshLegacy(space)
      else refreshAccount(space.account_id)
    },
    resetSession: () => {
      for (const accountId of knownSpaces.keys()) removeAccount(accountId)
      if (selected.peek() !== null) selected.value = null
    },
  }
}
