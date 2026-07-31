import { effect, signal, type Signal } from '@preact/signals'

/**
 * Schema-versioned client settings over `localStorage` (ADR 0046, M-W3).
 *
 * One key holds one JSON envelope with an explicit `version`, so future
 * shapes migrate deliberately instead of half-parsing old data. Anything
 * unparseable — missing, corrupt, an unknown future version — resets to
 * defaults rather than wedging the app: settings are preferences, not data.
 * New fields are added with defaults (an old envelope missing them still
 * parses); the version number bumps only on incompatible reshapes.
 */
const STORAGE_KEY = 'axon.settings'

export type Theme = 'system' | 'light' | 'dark'

/** Room-list sort modes (ADR 0042). */
export type RoomSort = 'recent' | 'oldest' | 'az' | 'za'

/**
 * Room-list filter categories (ADR 0042). The name filter is deliberately
 * absent: `Name(query)` is session-only and persists as `all`, since
 * restoring a stale query string is surprising.
 */
export type RoomFilter = 'all' | 'dms' | 'groups' | 'unread' | 'favorites'

/**
 * Timeline timestamp format. `12h` is the shipped default (kept from the
 * hardcoded format it replaces); `24h` restores what locale-aware rendering
 * used to give 24-hour-clock locales.
 */
export type TimeFormat = '12h' | '24h'

/**
 * How much of the state-event stream the timeline shows (ADR 0083).
 * `important` is membership and profile changes only — joins, leaves, invites,
 * kicks, display-name changes; `all` adds topic, power-level, ACL and the rest
 * of the room-configuration traffic.
 */
export type StateEventVisibility = 'hidden' | 'important' | 'all'

/** Desktop sidebar bounds in CSS pixels. Keep the room list readable. */
export const SIDEBAR_WIDTH_MIN = 360
export const SIDEBAR_WIDTH_MAX = 640

/** Version 1 settings envelope, the shape at rest in `localStorage`. */
export interface SettingsV1 {
  version: 1
  /** Color scheme; `system` follows `prefers-color-scheme`. */
  theme: Theme
  /**
   * The account the UI is "in" (account switch). Account-scoped routes carry
   * the account id in the URL; this is only the default for entry points
   * that don't, and it may point at an account that no longer exists —
   * consumers must treat it as a hint, not a fact.
   */
  activeAccountId: string | null
  /**
   * Pinned rooms (ADR 0038), each a room key (`accountId/roomId`, see
   * `stores/room-list.ts`), most recently pinned first. May reference rooms
   * that no longer exist; the room list simply won't match them.
   */
  pinnedRooms: string[]
  /** Personal ordering for the joined-space picker, keyed like pinned rooms. */
  spaceOrder: string[]
  /** Whether the desktop sidebar hides the spaces avatar rail. */
  spacesPaneCollapsed: boolean
  /** Keep the rail hidden while there is no meaningful space choice. */
  spacesPaneAutoHide: boolean
  /** Browser-local desktop sidebar width in CSS pixels. */
  sidebarWidth: number
  /** Persisted room-list sort mode (ADR 0042). */
  roomSort: RoomSort
  /** Persisted room-list filter category (ADR 0042). */
  roomFilter: RoomFilter
  /**
   * Whether the room-list sidebar is hidden (ADR 0062). Only consulted at
   * viewports wide enough for two panes; below that the route decides which
   * pane shows, and a stale `true` here must not hide the room list.
   */
  sidebarCollapsed: boolean
  /**
   * How much of the state-event stream the timeline shows (ADR 0083). A
   * preference, not per-room view state: it used to be an ephemeral checkbox in
   * the room header that reset on every room switch and reload. Defaults to
   * `important`, matching the TUI, which always renders membership events and
   * gates only the rest behind its `show_state_events` toggle.
   *
   * Replaces the older boolean `showStateEvents`; see `parse` for the
   * migration.
   */
  stateEvents: StateEventVisibility
  /** Whether redacted timeline events are hidden entirely. Off by default. */
  hideRedactedEvents: boolean
  /** Whether room-list rows show a one-line latest-message preview. */
  previewRoom: boolean
  /** Timeline timestamp format (Settings → Timeline). */
  timeFormat: TimeFormat
  /** User-sized message composer height in CSS pixels; null means default. */
  messageComposerHeight: number | null
  /**
   * Whether this browser has opted into registering Axon as a `matrix:`
   * protocol handler. The browser owns actual registration permission; this
   * only records the user's preference after a successful registration call.
   */
  matrixProtocolHandler: boolean
  /** Most recently used reaction keys, newest first. */
  recentReactions: string[]
  /**
   * Developer diagnostics: exposes per-event inspect actions in the timeline.
   * Off by default because event content can include decrypted message data.
   */
  developerMode: boolean
  /**
   * Emit the `performance.mark` instrumentation and draw its on-screen
   * readout. A development aid: the marks are cheap but not free, and the
   * readout sits over the app. Previously reachable only by hand-editing the
   * URL (`?perf=1`), which is fine for a harness and hostile on a phone.
   */
  perfMarks: boolean
  /**
   * Whether the installed app's icon shows a badge with the number of unread
   * messages (ADR 0080, Badging API). On by default, matching how other
   * messaging apps badge without asking: the API needs no permission prompt
   * and touches nothing off-device. Also on by default because there is no
   * way for this preference to reach a fresh install anyway — iOS gives a
   * newly added home-screen web app its own storage, separate from the
   * Safari tab it was added from, so a setting toggled beforehand can never
   * carry over.
   */
  appBadgeEnabled: boolean
  /**
   * Whether the room list is kept in a durable on-device cache so it paints
   * before the network answers (ADR 0085 phase 2). On by default: it holds
   * room *metadata* — names, topics, aliases, avatars, unread counts — of the
   * same kind already persisted under `axon.room_titles.v1`, and it is where
   * essentially all of the measured benefit lives (a 1,298 ms server-side wait
   * on the account ADR 0085 measured). Message bodies are a separate,
   * opt-in decision in phase 3; nothing here writes them.
   */
  cacheRoomList: boolean
}

const DEFAULTS: SettingsV1 = {
  version: 1,
  theme: 'system',
  activeAccountId: null,
  pinnedRooms: [],
  spaceOrder: [],
  spacesPaneCollapsed: false,
  spacesPaneAutoHide: true,
  sidebarWidth: 420,
  roomSort: 'recent',
  roomFilter: 'all',
  sidebarCollapsed: false,
  stateEvents: 'important',
  hideRedactedEvents: false,
  previewRoom: true,
  timeFormat: '12h',
  messageComposerHeight: null,
  matrixProtocolHandler: false,
  recentReactions: [],
  developerMode: false,
  perfMarks: false,
  appBadgeEnabled: true,
  cacheRoomList: true,
}

const MAX_RECENT_REACTIONS = 3

const THEMES: readonly Theme[] = ['system', 'light', 'dark']

const TIME_FORMATS: readonly TimeFormat[] = ['12h', '24h']

export const STATE_EVENT_VISIBILITIES: readonly StateEventVisibility[] = [
  'hidden',
  'important',
  'all',
]

/**
 * Cycle order for the sort shortcut, matching the TUI's `RoomSort::next`
 * (`clients/tui/src/app.rs`) so the two clients step in the same sequence.
 */
export const ROOM_SORTS: readonly RoomSort[] = ['recent', 'oldest', 'az', 'za']

/**
 * Cycle order for the filter shortcut, matching the TUI's `RoomFilter::CYCLE`
 * (`clients/tui/src/app.rs`). The name filter is deliberately outside the
 * cycle, exactly as in the TUI (ADR 0042).
 */
export const ROOM_FILTERS: readonly RoomFilter[] = [
  'all',
  'dms',
  'groups',
  'unread',
  'favorites',
]

/** The next value after `current`, wrapping. Unknown values restart the cycle. */
export function nextIn<T>(cycle: readonly T[], current: T): T {
  const index = cycle.indexOf(current)
  return cycle[(index + 1) % cycle.length]
}

/** Keep `value` when it is one of `allowed`, else the default. */
function oneOf<T extends string>(
  allowed: readonly T[],
  value: unknown,
  fallback: T,
): T {
  return allowed.includes(value as T) ? (value as T) : fallback
}

/** Parse a stored envelope, falling back to defaults on any irregularity. */
function parse(raw: string | null): SettingsV1 {
  if (raw === null) {
    return DEFAULTS
  }
  let value: unknown
  try {
    value = JSON.parse(raw)
  } catch {
    return DEFAULTS
  }
  if (
    typeof value !== 'object' ||
    value === null ||
    (value as { version?: unknown }).version !== 1
  ) {
    return DEFAULTS
  }
  const v1 = value as Partial<SettingsV1>
  return {
    version: 1,
    theme: oneOf(THEMES, v1.theme, DEFAULTS.theme),
    activeAccountId:
      typeof v1.activeAccountId === 'string' ? v1.activeAccountId : null,
    pinnedRooms: Array.isArray(v1.pinnedRooms)
      ? v1.pinnedRooms.filter((key): key is string => typeof key === 'string')
      : [],
    spaceOrder: Array.isArray(v1.spaceOrder)
      ? v1.spaceOrder.filter((key): key is string => typeof key === 'string')
      : [],
    spacesPaneCollapsed:
      typeof v1.spacesPaneCollapsed === 'boolean'
        ? v1.spacesPaneCollapsed
        : DEFAULTS.spacesPaneCollapsed,
    spacesPaneAutoHide:
      typeof v1.spacesPaneAutoHide === 'boolean'
        ? v1.spacesPaneAutoHide
        : DEFAULTS.spacesPaneAutoHide,
    sidebarWidth:
      typeof v1.sidebarWidth === 'number' &&
      Number.isFinite(v1.sidebarWidth) &&
      v1.sidebarWidth >= SIDEBAR_WIDTH_MIN &&
      v1.sidebarWidth <= SIDEBAR_WIDTH_MAX
        ? Math.round(v1.sidebarWidth)
        : DEFAULTS.sidebarWidth,
    roomSort: oneOf(ROOM_SORTS, v1.roomSort, DEFAULTS.roomSort),
    roomFilter: oneOf(ROOM_FILTERS, v1.roomFilter, DEFAULTS.roomFilter),
    sidebarCollapsed:
      typeof v1.sidebarCollapsed === 'boolean'
        ? v1.sidebarCollapsed
        : DEFAULTS.sidebarCollapsed,
    // Migration from the pre-0079 boolean, in place rather than behind a
    // version bump: bumping resets the *whole* envelope, and losing someone's
    // theme and pinned rooms to a timeline-filter reshape is a bad trade. An
    // envelope written by an older build carries `showStateEvents` and no
    // `stateEvents`; opting into the firehose maps to `all`, and everyone else
    // lands on the new default tier. The legacy key is simply not re-persisted.
    stateEvents: oneOf(
      STATE_EVENT_VISIBILITIES,
      v1.stateEvents,
      (value as { showStateEvents?: unknown }).showStateEvents === true
        ? 'all'
        : DEFAULTS.stateEvents,
    ),
    hideRedactedEvents:
      typeof v1.hideRedactedEvents === 'boolean'
        ? v1.hideRedactedEvents
        : DEFAULTS.hideRedactedEvents,
    previewRoom:
      typeof v1.previewRoom === 'boolean'
        ? v1.previewRoom
        : DEFAULTS.previewRoom,
    timeFormat: oneOf(TIME_FORMATS, v1.timeFormat, DEFAULTS.timeFormat),
    messageComposerHeight:
      typeof v1.messageComposerHeight === 'number' &&
      Number.isFinite(v1.messageComposerHeight) &&
      v1.messageComposerHeight >= 38
        ? Math.round(v1.messageComposerHeight)
        : DEFAULTS.messageComposerHeight,
    matrixProtocolHandler:
      typeof v1.matrixProtocolHandler === 'boolean'
        ? v1.matrixProtocolHandler
        : DEFAULTS.matrixProtocolHandler,
    recentReactions: Array.isArray(v1.recentReactions)
      ? v1.recentReactions
          .filter((key): key is string => typeof key === 'string')
          .filter((key) => key.trim() !== '')
          .slice(0, MAX_RECENT_REACTIONS)
      : [],
    developerMode:
      typeof v1.developerMode === 'boolean'
        ? v1.developerMode
        : DEFAULTS.developerMode,
    perfMarks:
      typeof v1.perfMarks === 'boolean' ? v1.perfMarks : DEFAULTS.perfMarks,
    appBadgeEnabled:
      typeof v1.appBadgeEnabled === 'boolean'
        ? v1.appBadgeEnabled
        : DEFAULTS.appBadgeEnabled,
    cacheRoomList:
      typeof v1.cacheRoomList === 'boolean'
        ? v1.cacheRoomList
        : DEFAULTS.cacheRoomList,
  }
}

export interface SettingsStore {
  theme: Signal<Theme>
  activeAccountId: Signal<string | null>
  pinnedRooms: Signal<string[]>
  spaceOrder: Signal<string[]>
  spacesPaneCollapsed: Signal<boolean>
  spacesPaneAutoHide: Signal<boolean>
  sidebarWidth: Signal<number>
  roomSort: Signal<RoomSort>
  roomFilter: Signal<RoomFilter>
  sidebarCollapsed: Signal<boolean>
  stateEvents: Signal<StateEventVisibility>
  hideRedactedEvents: Signal<boolean>
  previewRoom: Signal<boolean>
  timeFormat: Signal<TimeFormat>
  messageComposerHeight: Signal<number | null>
  matrixProtocolHandler: Signal<boolean>
  recentReactions: Signal<string[]>
  developerMode: Signal<boolean>
  perfMarks: Signal<boolean>
  appBadgeEnabled: Signal<boolean>
  cacheRoomList: Signal<boolean>
  /**
   * Pin a room key, or re-pin an already-pinned one to the top — most
   * recently pinned first (ADR 0038).
   */
  pinRoom(key: string): void
  /** Unpin a room key; a no-op when it isn't pinned. */
  unpinRoom(key: string): void
  /**
   * Move a space key to a new position in the browser-local picker.
   *
   * `visibleKeys` is the picker's full displayed order, which `toIndex` indexes
   * into. The persisted order only holds keys the user has already moved, so it
   * has to be materialized against the displayed order before splicing —
   * clamping `toIndex` against the stored array alone silently turned every
   * downward move into an insert near the top.
   */
  moveSpace(key: string, toIndex: number, visibleKeys: readonly string[]): void
  /** Record a reaction key as recently used, newest first. */
  recordRecentReaction(key: string): void
}

/**
 * Load settings and keep every change persisted. Storage is injectable for
 * tests (jsdom under Node 25 has no working `localStorage`).
 */
export function createSettingsStore(
  storage: Storage = window.localStorage,
): SettingsStore {
  const initial = parse(storage.getItem(STORAGE_KEY))
  const theme = signal<Theme>(initial.theme)
  const activeAccountId = signal<string | null>(initial.activeAccountId)
  const pinnedRooms = signal<string[]>(initial.pinnedRooms)
  const spaceOrder = signal<string[]>(initial.spaceOrder)
  const spacesPaneCollapsed = signal<boolean>(initial.spacesPaneCollapsed)
  const spacesPaneAutoHide = signal<boolean>(initial.spacesPaneAutoHide)
  const sidebarWidth = signal<number>(initial.sidebarWidth)
  const roomSort = signal<RoomSort>(initial.roomSort)
  const roomFilter = signal<RoomFilter>(initial.roomFilter)
  const sidebarCollapsed = signal<boolean>(initial.sidebarCollapsed)
  const stateEvents = signal<StateEventVisibility>(initial.stateEvents)
  const hideRedactedEvents = signal<boolean>(initial.hideRedactedEvents)
  const previewRoom = signal<boolean>(initial.previewRoom)
  const timeFormat = signal<TimeFormat>(initial.timeFormat)
  const messageComposerHeight = signal<number | null>(
    initial.messageComposerHeight,
  )
  const matrixProtocolHandler = signal<boolean>(initial.matrixProtocolHandler)
  const recentReactions = signal<string[]>(initial.recentReactions)
  const developerMode = signal<boolean>(initial.developerMode)
  const perfMarks = signal<boolean>(initial.perfMarks)
  const appBadgeEnabled = signal<boolean>(initial.appBadgeEnabled)
  const cacheRoomList = signal<boolean>(initial.cacheRoomList)

  effect(() => {
    const envelope: SettingsV1 = {
      version: 1,
      theme: theme.value,
      activeAccountId: activeAccountId.value,
      pinnedRooms: pinnedRooms.value,
      spaceOrder: spaceOrder.value,
      spacesPaneCollapsed: spacesPaneCollapsed.value,
      spacesPaneAutoHide: spacesPaneAutoHide.value,
      sidebarWidth: sidebarWidth.value,
      roomSort: roomSort.value,
      roomFilter: roomFilter.value,
      sidebarCollapsed: sidebarCollapsed.value,
      stateEvents: stateEvents.value,
      hideRedactedEvents: hideRedactedEvents.value,
      previewRoom: previewRoom.value,
      timeFormat: timeFormat.value,
      messageComposerHeight: messageComposerHeight.value,
      matrixProtocolHandler: matrixProtocolHandler.value,
      recentReactions: recentReactions.value,
      developerMode: developerMode.value,
      perfMarks: perfMarks.value,
      appBadgeEnabled: appBadgeEnabled.value,
      cacheRoomList: cacheRoomList.value,
    }
    try {
      storage.setItem(STORAGE_KEY, JSON.stringify(envelope))
    } catch {
      // Quota or storage-denied: settings are preferences — losing a persist
      // must not throw into whatever signal write triggered this effect.
    }
  })

  return {
    theme,
    activeAccountId,
    pinnedRooms,
    spaceOrder,
    spacesPaneCollapsed,
    spacesPaneAutoHide,
    sidebarWidth,
    roomSort,
    roomFilter,
    sidebarCollapsed,
    stateEvents,
    hideRedactedEvents,
    previewRoom,
    timeFormat,
    messageComposerHeight,
    matrixProtocolHandler,
    recentReactions,
    developerMode,
    perfMarks,
    appBadgeEnabled,
    cacheRoomList,
    pinRoom(key: string) {
      pinnedRooms.value = [key, ...pinnedRooms.value.filter((k) => k !== key)]
    },
    unpinRoom(key: string) {
      pinnedRooms.value = pinnedRooms.value.filter((k) => k !== key)
    },
    moveSpace(key: string, toIndex: number, visibleKeys: readonly string[]) {
      // Keys the picker no longer shows stay ranked, so a space that is briefly
      // absent (an account still syncing) keeps its place.
      const hidden = spaceOrder.value.filter(
        (candidate) => candidate !== key && !visibleKeys.includes(candidate),
      )
      const current = visibleKeys.filter((candidate) => candidate !== key)
      const index = Math.max(0, Math.min(toIndex, current.length))
      current.splice(index, 0, key)
      spaceOrder.value = [...current, ...hidden]
    },
    recordRecentReaction(key: string) {
      const trimmed = key.trim()
      if (trimmed === '') {
        return
      }
      recentReactions.value = [
        trimmed,
        ...recentReactions.value.filter((k) => k !== trimmed),
      ].slice(0, MAX_RECENT_REACTIONS)
    },
  }
}

/**
 * Reflect the theme onto `<html data-theme="…">`, where the CSS lives.
 * `system` removes the attribute so `prefers-color-scheme` decides.
 */
export function applyTheme(
  store: SettingsStore,
  root: HTMLElement,
): () => void {
  return effect(() => {
    if (store.theme.value === 'system') {
      root.removeAttribute('data-theme')
    } else {
      root.setAttribute('data-theme', store.theme.value)
    }
  })
}
