import { useLocation } from 'preact-iso'
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from 'preact/hooks'
import { layoutMode } from '../layout'
import { localRoomHref } from '../matrix-to'
import { useMediaBlob } from '../media/use-media-blob'
import { useServices } from '../services'
import { ErrorBanner } from './ErrorBanner'
import { perfMark, perfMarkFrames } from '../perf'
import {
  hasModifier,
  hint,
  isApplePlatform,
  keyAria,
  keyLabel,
  KEYS,
  useShortcuts,
} from '../shortcuts'
import { SLASH_COMMAND } from '../slash-commands'
import {
  accountLabels,
  filterRooms,
  isLikelyDm,
  roomKey,
  roomTitle,
  sortRooms,
  type ActiveFilter,
  type RoomDto,
} from '../stores/room-list'
import {
  nextIn,
  ROOM_FILTERS,
  ROOM_SORTS,
  type RoomFilter,
  type RoomSort,
} from '../stores/settings'
import { rowWindow, scrollToReveal } from './virtual-window'

const FILTERS: { value: RoomFilter; label: string; shortLabel: string }[] = [
  { value: 'all', label: 'All rooms', shortLabel: 'All' },
  { value: 'dms', label: 'DMs', shortLabel: 'DM' },
  { value: 'groups', label: 'Groups', shortLabel: 'Group' },
  { value: 'unread', label: 'Unread', shortLabel: 'Unread' },
  { value: 'favorites', label: 'Favorites', shortLabel: 'Favs' },
]

const SORTS: { value: RoomSort; label: string }[] = [
  { value: 'recent', label: 'Recent activity' },
  { value: 'oldest', label: 'Oldest activity' },
  { value: 'az', label: 'A–Z' },
  { value: 'za', label: 'Z–A' },
]

/** Rows rendered beyond each edge of the viewport, so a scroll or an arrow
 *  keypress lands on a row that already exists. */
const OVERSCAN = 6

/** Row pitch assumed for the first paint, before the DOM can be measured. It
 *  only has to be close: the real pitch replaces it in the same frame. */
const ESTIMATED_ROW_PITCH = 64

/** Viewport assumed for the single render before the scroll host is known.
 *  Generous enough to fill a tall screen, bounded enough that a first paint
 *  never mounts thousands of rows. Corrected before paint. */
const ASSUMED_VIEWPORT = 1200
const TOUCH_TAP_SLOP_PX = 10

const EMPTY_UNREAD_KEYS: ReadonlySet<string> = new Set()

/** The nearest scrolling ancestor, or `null` when nothing scrolls (a
 *  layout-less test environment, or the list rendered outside the sidebar). */
function scrollParentOf(element: HTMLElement | null): HTMLElement | null {
  for (
    let node = element?.parentElement ?? null;
    node !== null;
    node = node.parentElement
  ) {
    const { overflowY } = getComputedStyle(node)
    if (overflowY === 'auto' || overflowY === 'scroll') {
      return node
    }
  }
  return null
}

function roomAvatarLabel(title: string): string {
  const trimmed = title.trim()
  return trimmed === '' ? '?' : trimmed[0].toLocaleUpperCase()
}

function roomAvatarColor(roomKeyValue: string): number {
  let hash = 0
  for (let index = 0; index < roomKeyValue.length; index += 1) {
    hash = (hash * 31 + roomKeyValue.charCodeAt(index)) >>> 0
  }
  return hash % 8
}

/**
 * The room list (ADR 0046, M-W4): TUI semantics — pinned section on top in
 * pin order (ADR 0038), sort modes and category/name filters (ADR 0042),
 * server-derived unread badges (ADR 0070), membership filtering
 * server-side (ADR 0037). Rows navigate to the deep-link URL shape.
 *
 * Mounted once in the app shell as the left pane (ADR 0062) and hidden with
 * CSS rather than unmounted, so the session-only filters below and the list's
 * scroll position survive every room switch.
 */
export function RoomList() {
  const {
    accounts: accountStore,
    rooms,
    settings,
    spaces,
    activeRoom,
    composerFocus,
  } = useServices()
  const location = useLocation()
  // Session-only name filter (ADR 0042: never persisted). `null` = category
  // mode; a string means the name input is active and overrides the category.
  const [nameQuery, setNameQuery] = useState<string | null>(null)
  // Session-only account narrowing, the TUI's always-applied outer filter.
  const [accountFilter, setAccountFilter] = useState<string>('all')
  const filterInput = useRef<HTMLInputElement>(null)
  const list = useRef<HTMLUListElement>(null)
  const scroller = useRef<HTMLElement | null>(null)
  const actionsMenu = useRef<HTMLDetailsElement>(null)
  const firstRoomAction = useRef<HTMLAnchorElement>(null)

  useEffect(() => {
    void rooms.refresh()
  }, [rooms])
  useEffect(() => {
    if (accountStore.loading.value) {
      void accountStore.refresh()
    }
  }, [accountStore, accountStore.loading.value])
  useEffect(() => {
    if (actionsMenu.current !== null) {
      actionsMenu.current.open = false
    }
  }, [location.url])
  useEffect(() => {
    const closeActionsMenuOnOutsidePointer = (event: PointerEvent) => {
      const menu = actionsMenu.current
      if (
        menu === null ||
        !menu.open ||
        (event.target instanceof Node && menu.contains(event.target))
      ) {
        return
      }
      menu.open = false
    }
    document.addEventListener('pointerdown', closeActionsMenuOnOutsidePointer)
    return () =>
      document.removeEventListener(
        'pointerdown',
        closeActionsMenuOnOutsidePointer,
      )
  }, [])

  const allRooms = rooms.rooms.value
  const selectedSpace = spaces.selected.value
  const selectedChildren =
    selectedSpace === null
      ? null
      : (spaces.children.value.get(selectedSpace) ?? null)
  // A space whose children have not loaded must not read as "this space
  // contains everything": until the fetch resolves the scope is unknown, so the
  // list shows why rather than silently falling back to the account filter.
  const spaceScope: 'none' | 'ready' | 'loading' | 'failed' =
    selectedSpace === null
      ? 'none'
      : selectedChildren !== null
        ? 'ready'
        : spaces.errors.value.has(selectedSpace)
          ? 'failed'
          : 'loading'
  const spaceScopeError =
    selectedSpace === null ? undefined : spaces.errors.value.get(selectedSpace)
  const selectedSpaceAccount =
    selectedSpace === null ? null : selectedSpace.split('/', 1)[0]
  const selectedSpaceRoom =
    selectedSpace === null
      ? undefined
      : allRooms.find((room) => roomKey(room) === selectedSpace)
  const accounts = accountStore.accounts.value
  const activeAccounts = useMemo(
    () => accounts.filter((account) => account.state === 'active'),
    [accounts],
  )
  const activeAccountEntries = useMemo(
    () =>
      activeAccounts.map(
        (account) => [account.account_id, account.user_id] as const,
      ),
    [activeAccounts],
  )
  const activeAccountIds = useMemo(
    () => new Set(activeAccountEntries.map(([id]) => id)),
    [activeAccountEntries],
  )
  const activeAccountsKnown =
    !accountStore.loading.value && accountStore.error.value === null
  const scopedRooms = allRooms.filter(
    (room) =>
      room.room_type !== 'm.space' &&
      (!activeAccountsKnown || activeAccountIds.has(room.account_id)),
  )
  const showAccountFilter = activeAccountEntries.length > 1
  const labels = useMemo(
    () => accountLabels(activeAccountEntries),
    [activeAccountEntries],
  )
  const roomFilter = settings.roomFilter.value
  const roomSort = settings.roomSort.value
  const pinnedRooms = settings.pinnedRooms.value
  const roomTitles = rooms.titles.value
  const unreadKeys =
    nameQuery === null && roomFilter === 'unread'
      ? rooms.unreadKeys.value
      : EMPTY_UNREAD_KEYS
  const navigateToRoom = useCallback(
    (room: RoomDto, href: string) => {
      const key = roomKey(room)
      perfMark('room-list:navigate-to-room', { key })
      rooms.noteUnreadCounts(room.account_id, room.room_id, 0, 0)
      location.route(href)
    },
    [location, rooms],
  )
  const visible = useMemo(() => {
    perfMark('room-list:visible-compute:start', {
      rooms: scopedRooms.length,
      filter: roomFilter,
      sort: roomSort,
      name: nameQuery !== null,
      unreadRooms: unreadKeys.size,
    })
    const next = visibleRooms(scopedRooms, {
      accountFilter: selectedSpaceAccount ?? accountFilter,
      nameQuery,
      roomFilter,
      roomSort,
      pinnedRooms,
      roomTitles,
      hasUnread: (room) => unreadKeys.has(roomKey(room)),
    })
    if (selectedChildren === null) {
      const visible = spaceScope === 'none' ? next : []
      perfMark('room-list:visible-compute:end', { visible: visible.length })
      return visible
    }
    const childIds = new Set(selectedChildren.map((child) => child.room_id))
    const scoped = next.filter(
      (room) =>
        room.account_id === selectedSpaceAccount && childIds.has(room.room_id),
    )
    perfMark('room-list:visible-compute:end', { visible: scoped.length })
    return scoped
  }, [
    scopedRooms,
    accountFilter,
    nameQuery,
    roomFilter,
    roomSort,
    pinnedRooms,
    roomTitles,
    unreadKeys,
    selectedSpaceAccount,
    selectedChildren,
    spaceScope,
  ])
  useEffect(() => {
    if (
      accountFilter !== 'all' &&
      (!showAccountFilter || !activeAccountIds.has(accountFilter))
    ) {
      setAccountFilter('all')
    }
  }, [accountFilter, activeAccountIds, showAccountFilter])
  const pinnedCount = useMemo(
    () => visible.filter((room) => pinnedRooms.includes(roomKey(room))).length,
    [visible, pinnedRooms],
  )

  // ---- windowing (see `virtual-window.ts`). Geometry is measured, never
  // assumed: `pitch` comes from two live rows, the viewport from the scrolling
  // ancestor.
  const [pitch, setPitch] = useState(ESTIMATED_ROW_PITCH)
  const [scrollTop, setScrollTop] = useState(0)
  const [viewport, setViewport] = useState(0)
  const [pendingFocus, setPendingFocus] = useState<number | null>(null)
  const nameFilterValue = nameQuery ?? ''

  /**
   * Whether the list has a scrolling ancestor. `unknown` on the first render —
   * refs are not populated yet — and resolved before paint by the layout effect
   * below. `none` means nothing scrolls (jsdom, or the list mounted outside the
   * sidebar) and every row is rendered; `found` hands the decision to
   * `rowWindow`, which renders nothing while that ancestor is hidden.
   */
  const [scrollHost, setScrollHost] = useState<'unknown' | 'none' | 'found'>(
    'unknown',
  )

  const measure = useCallback(() => {
    const container = scroller.current
    const ul = list.current
    if (container === null || ul === null) {
      return
    }
    perfMark('room-list:measure:start')
    setViewport(container.clientHeight)
    // How much of the list has scrolled past the container's top edge. The
    // list's border-box top does not move when its padding changes, so this
    // stays honest as the window's spacers resize.
    const offset =
      container.getBoundingClientRect().top - ul.getBoundingClientRect().top
    setScrollTop(Math.max(0, offset))
    perfMark('room-list:measure:end', {
      viewport: container.clientHeight,
      scrollTop: Math.max(0, offset),
    })
  }, [])

  // Re-attach whenever the list element itself is replaced (it unmounts while
  // the store loads and when no room matches the filter).
  const hasRows = !rooms.loading.value && visible.length > 0
  useLayoutEffect(() => {
    perfMark('room-list:scroll-host-effect:start', {
      hasRows,
      visible: visible.length,
    })
    if (!hasRows) {
      scroller.current = null
      perfMark('room-list:scroll-host-effect:no-rows')
      return
    }
    const container = scrollParentOf(list.current)
    scroller.current = container
    setScrollHost(container === null ? 'none' : 'found')
    if (container === null) {
      perfMark('room-list:scroll-host-effect:no-container')
      return
    }
    measure()
    perfMark('room-list:scroll-host-effect:end')
    let frame = 0
    const onScroll = () => {
      // Coalesce a burst of scroll events into one measurement per frame.
      if (frame === 0) {
        frame = requestAnimationFrame(() => {
          frame = 0
          measure()
        })
      }
    }
    container.addEventListener('scroll', onScroll, { passive: true })
    const observer =
      typeof ResizeObserver === 'undefined'
        ? null
        : new ResizeObserver(() => measure())
    observer?.observe(container)
    return () => {
      if (frame !== 0) {
        cancelAnimationFrame(frame)
      }
      container.removeEventListener('scroll', onScroll)
      observer?.disconnect()
    }
  }, [hasRows, measure, visible.length])

  // Measure the row pitch from two adjacent rows — their height plus whatever
  // rule or gap separates them. Both are windowed rows, so this holds at any
  // scroll offset. A single row (or none) leaves the estimate in place.
  //
  // Re-measuring on `pitch` is what converges it: the first paint uses the
  // estimate, this corrects it, and the corrected pass finds nothing to change.
  // `viewport` catches a zoom or font-size change, which moves the pitch and
  // resizes the container together.
  useLayoutEffect(() => {
    perfMark('room-list:pitch-effect:start', { pitch, hasRows, viewport })
    const rows = list.current?.querySelectorAll<HTMLElement>('li.room-row')
    if (rows === undefined || rows.length < 2) {
      perfMark('room-list:pitch-effect:skip', { rows: rows?.length ?? 0 })
      return
    }
    const measured = rows[1].offsetTop - rows[0].offsetTop
    if (measured > 0 && Math.abs(measured - pitch) > 0.5) {
      perfMark('room-list:pitch-effect:update', { measured })
      setPitch(measured)
    }
    perfMark('room-list:pitch-effect:end', { measured })
  }, [pitch, hasRows, viewport])

  // `none` cannot window at all. `unknown` is the single pre-measure render:
  // it guesses a screenful rather than mounting every row, because the layout
  // effect that resolves it runs *after* this render has built its DOM.
  const { start, end, padTop, padBottom } =
    scrollHost === 'none'
      ? { start: 0, end: visible.length, padTop: 0, padBottom: 0 }
      : rowWindow(
          visible.length,
          pitch,
          scrollHost === 'unknown' ? 0 : scrollTop,
          scrollHost === 'unknown' ? ASSUMED_VIEWPORT : viewport,
          OVERSCAN,
        )
  perfMark('room-list:render', {
    visible: visible.length,
    start,
    end,
    scrollHost,
    hasRows,
  })
  useEffect(() => {
    perfMarkFrames('room-list:post-render')
  })

  /** Scroll row `index` into view, updating our own offset in the same tick so
   *  the next render already contains it (the scroll event lands later). */
  const revealRow = useCallback(
    (index: number) => {
      const container = scroller.current
      if (container === null) {
        return
      }
      const next = scrollToReveal(index, pitch, scrollTop, viewport)
      if (next === null) {
        return
      }
      container.scrollTop += next - scrollTop
      setScrollTop(next)
    },
    [pitch, scrollTop, viewport],
  )

  const linkAt = (index: number) =>
    list.current?.querySelector<HTMLAnchorElement>(
      `a.room-link[data-index="${index}"]`,
    ) ?? null

  const focusRow = (index: number) => {
    revealRow(index)
    const link = linkAt(index)
    if (link === null) {
      // Outside the window: `revealRow` has moved it in, so focus it once the
      // render that includes it has landed.
      setPendingFocus(index)
      return
    }
    link.focus()
    // jsdom has no scrollIntoView; the optional call keeps tests honest.
    link.scrollIntoView?.({ block: 'nearest' })
  }

  useEffect(() => {
    if (pendingFocus === null) {
      return
    }
    linkAt(pendingFocus)?.focus()
    // Cleared unconditionally: a row that still is not rendered is one we
    // cannot reach, and retrying would spin.
    setPendingFocus(null)
  }, [pendingFocus, start, end])

  // Ctrl-K may have to un-collapse the sidebar or leave a utility page first,
  // and `focus()` on a `display: none` element is silently a no-op. Defer to a
  // macrotask, which lands after the signal flush has re-rendered the shell —
  // `requestAnimationFrame` is not a safe substitute, since a page that is not
  // painting (a background tab, a headless browser) may not run it for a long
  // time. The flag clears inside the callback so the effect's cleanup cannot
  // cancel the timeout it just scheduled.
  const [focusFilter, setFocusFilter] = useState(false)
  useEffect(() => {
    if (!focusFilter) {
      return
    }
    const timer = setTimeout(() => {
      filterInput.current?.focus()
      filterInput.current?.select()
      setFocusFilter(false)
    })
    return () => clearTimeout(timer)
  }, [focusFilter])

  /**
   * Arrow keys rove real DOM focus across the room links rather than tracking a
   * selected index: `Enter` then activates the anchor natively, and screen
   * readers announce each room as it is reached. `ArrowUp` off the top returns
   * to the filter input, which is where the sequence began.
   *
   * The roved position is the focused row's index into `visible`, read off the
   * DOM, because only the windowed rows exist to be focused — stepping past the
   * window's edge scrolls the next row in rather than running out of links.
   */
  const moveFocus = (delta: number) => {
    if (visible.length === 0) {
      return
    }
    const active = document.activeElement
    const attribute =
      active instanceof HTMLElement && active.classList.contains('room-link')
        ? active.getAttribute('data-index')
        : null
    const current = attribute === null ? -1 : Number(attribute)
    if (current === 0 && delta < 0) {
      filterInput.current?.focus()
      return
    }
    const index =
      current < 0
        ? delta > 0
          ? 0
          : visible.length - 1
        : Math.min(Math.max(current + delta, 0), visible.length - 1)
    focusRow(index)
  }

  /** Hand the keyboard back to the open room's composer (the TUI's Esc). */
  const focusComposer = () => {
    composerFocus.value += 1
  }

  const activateRoom = (room: RoomDto, href?: string) => {
    navigateToRoom(
      room,
      href ?? `/${room.account_id}/rooms/${encodeURIComponent(room.room_id)}`,
    )
    setTimeout(focusComposer)
  }

  /**
   * Jump to the room `delta` away from the open one, wrapping at the ends.
   *
   * Stepping from the composer leaves the keyboard in the composer: the user
   * was typing, and the room changing under them should not cost them the
   * caret. The composer is keyed by room id, so the new room mounts a fresh one
   * whose focus baseline starts at the current `composerFocus` — bumping before
   * the route lands would therefore be swallowed. Defer past the mount, the
   * same reason Ctrl-K defers.
   */
  const stepRoom = (delta: number) => {
    if (visible.length === 0) {
      return
    }
    const fromComposer =
      document.activeElement instanceof Element &&
      document.activeElement.closest('.composer') !== null
    const current = visible.findIndex(
      (room) => roomKey(room) === activeRoom.value,
    )
    const index =
      current < 0
        ? delta > 0
          ? 0
          : visible.length - 1
        : (current + delta + visible.length) % visible.length
    const room = visible[index]
    rooms.noteUnreadCounts(room.account_id, room.room_id, 0, 0)
    // The row may be outside the window; keep the highlighted room on screen.
    revealRow(index)
    location.route(
      `/${room.account_id}/rooms/${encodeURIComponent(room.room_id)}`,
    )
    if (fromComposer) {
      setTimeout(focusComposer)
    }
  }

  useShortcuts(
    {
      'mod+k': (event) => {
        event.preventDefault()
        settings.sidebarCollapsed.value = false
        if (layoutMode(location.path) === 'utility') {
          location.route('/')
        }
        setFocusFilter(true)
      },
      // `mod+shift+r` is browser reload, so the filter cycle uses a nearby
      // browser-safe fallback chord instead.
      'mod+shift+y': (event) => {
        event.preventDefault()
        // The name filter is outside the cycle, as in the TUI (ADR 0042), so
        // cycling a category has to drop it.
        setNameQuery(null)
        settings.roomFilter.value = nextIn(
          ROOM_FILTERS,
          settings.roomFilter.value,
        )
      },
      'mod+shift+s': (event) => {
        event.preventDefault()
        settings.roomSort.value = nextIn(ROOM_SORTS, settings.roomSort.value)
      },
      'mod+arrowup': (event) => {
        if (isApplePlatform()) {
          return
        }
        event.preventDefault()
        stepRoom(-1)
      },
      'mod+arrowdown': (event) => {
        if (isApplePlatform()) {
          return
        }
        event.preventDefault()
        stepRoom(1)
      },
      'mod+alt+arrowup': (event) => {
        if (!isApplePlatform()) {
          return
        }
        event.preventDefault()
        stepRoom(-1)
      },
      'mod+alt+arrowdown': (event) => {
        if (!isApplePlatform()) {
          return
        }
        event.preventDefault()
        stepRoom(1)
      },
    },
    // Every chord here carries a modifier, so it is safe from the composer.
    { whileTyping: true },
  )
  useShortcuts({
    '+': (event) => {
      event.preventDefault()
      settings.sidebarCollapsed.value = false
      if (layoutMode(location.path) === 'utility') {
        location.route('/')
      }
      setTimeout(() => {
        if (actionsMenu.current === null) {
          return
        }
        actionsMenu.current.open = true
        firstRoomAction.current?.focus()
      })
    },
  })

  /**
   * Arrow/Escape handling shared by the filter input and the room links.
   *
   * Bare arrows only. Modified arrows are reserved for platform text
   * navigation and room stepping, and this handler runs first (it is bound
   * below `document`), so claiming a modified arrow here would rove focus
   * instead and hide the chord from `useShortcuts`, which ignores a
   * defaultPrevented event.
   */
  const onListKeyDown = (event: KeyboardEvent) => {
    if (hasModifier(event)) {
      return
    }
    if (event.key === 'ArrowDown') {
      event.preventDefault()
      moveFocus(1)
    } else if (event.key === 'ArrowUp') {
      event.preventDefault()
      moveFocus(-1)
    } else if (event.key === 'Escape') {
      event.preventDefault()
      if (event.target === filterInput.current && nameFilterValue !== '') {
        setNameQuery(null)
        return
      }
      focusComposer()
    } else if (event.key === 'Enter') {
      if (event.target === filterInput.current && visible.length === 1) {
        event.preventDefault()
        activateRoom(visible[0])
        return
      }
      const active =
        event.target instanceof HTMLElement &&
        event.target.classList.contains('room-link')
          ? event.target
          : null
      if (active === null) {
        return
      }
      const index = Number(active.dataset.index)
      const room = Number.isInteger(index) ? visible[index] : undefined
      const href = active.getAttribute('href')
      if (room === undefined || href === null) {
        return
      }
      event.preventDefault()
      activateRoom(room, href)
    }
  }
  return (
    <>
      <ErrorBanner error={rooms.error} />

      <div class="room-controls">
        {/* The chord cycles the group rather than activating any one chip, so
            `aria-keyshortcuts` belongs here and not on the buttons. */}
        <div
          class="filter-group"
          role="group"
          aria-label="Filter"
          aria-keyshortcuts={keyAria(KEYS.cycleFilter)}
        >
          {FILTERS.map(({ value, label, shortLabel }) => (
            <button
              key={value}
              type="button"
              aria-label={label}
              title={`Show ${label} — ${hint('cycle filters', KEYS.cycleFilter)}`}
              class={
                nameQuery === null && settings.roomFilter.value === value
                  ? 'chip active'
                  : 'chip'
              }
              onClick={() => {
                setNameQuery(null)
                settings.roomFilter.value = value
              }}
            >
              {shortLabel}
            </button>
          ))}
        </div>
        <span class="name-filter-shell">
          <input
            ref={filterInput}
            class="name-filter"
            type="search"
            placeholder="Filter by name…"
            aria-label="Filter by name"
            title={hint('Filter rooms by name', KEYS.filterRooms)}
            aria-keyshortcuts={keyAria(KEYS.filterRooms)}
            value={nameFilterValue}
            onInput={(event) => setNameQuery(event.currentTarget.value)}
            onBlur={() => nameQuery?.trim() === '' && setNameQuery(null)}
            onKeyDown={onListKeyDown}
          />
          {nameFilterValue !== '' && (
            <button
              type="button"
              class="name-filter-clear"
              aria-label="Clear room filter"
              title="Clear room filter"
              onClick={() => {
                setNameQuery(null)
                filterInput.current?.focus()
              }}
            >
              ×
            </button>
          )}
        </span>
        <div class="room-control-row">
          <label class="sort-select">
            Sort
            <select
              title={hint('Cycle sort order', KEYS.cycleSort)}
              aria-keyshortcuts={keyAria(KEYS.cycleSort)}
              value={settings.roomSort.value}
              onChange={(event) =>
                (settings.roomSort.value = event.currentTarget
                  .value as RoomSort)
              }
            >
              {SORTS.map(({ value, label }) => (
                <option key={value} value={value}>
                  {label}
                </option>
              ))}
            </select>
          </label>
          <details
            class="room-actions-menu"
            ref={actionsMenu}
            onKeyDown={(event) => {
              if (event.key === 'Escape' && actionsMenu.current?.open) {
                event.preventDefault()
                actionsMenu.current.open = false
              }
            }}
          >
            <summary
              title={`Room actions (${keyLabel(KEYS.roomActions)}; ${SLASH_COMMAND.join}, ${SLASH_COMMAND.dm}, ${SLASH_COMMAND.create}, ${SLASH_COMMAND.find})`}
              aria-label="Rooms"
              aria-keyshortcuts={keyAria(KEYS.roomActions)}
            >
              <span class="room-actions-plus" aria-hidden="true" />
              <span class="room-actions-label">Rooms</span>
            </summary>
            <div class="room-actions-popover">
              <a ref={firstRoomAction} href="/rooms/discover#join">
                Join
              </a>
              <a href="/rooms/dm#dm">DM</a>
              <a href="/rooms/create#create">Create</a>
              <a href="/rooms/discover#find">Find</a>
            </div>
          </details>
        </div>
        {showAccountFilter && (
          <label class="sort-select">
            Account
            <select
              value={accountFilter}
              onChange={(event) => setAccountFilter(event.currentTarget.value)}
            >
              <option value="all">All accounts</option>
              {activeAccountEntries.map(([id, userId]) => (
                <option key={id} value={id}>
                  {userId}
                </option>
              ))}
            </select>
          </label>
        )}
      </div>

      {selectedSpace !== null && (
        <div class="space-scope">
          <p class="space-scope-title">
            {selectedSpaceRoom === undefined
              ? 'Selected space'
              : roomTitle(selectedSpaceRoom, roomTitles)}
          </p>
          <div class="space-scope-actions">
            {/* Joined spaces are filtered out of the list below, so this is the
                only way to reach a space's own timeline, topic or leave
                action. */}
            {selectedSpaceRoom !== undefined && (
              <button
                type="button"
                class="ghost"
                onClick={() =>
                  navigateToRoom(
                    selectedSpaceRoom,
                    localRoomHref(
                      selectedSpaceRoom.account_id,
                      selectedSpaceRoom.room_id,
                      null,
                    ),
                  )
                }
              >
                Open space room
              </button>
            )}
            <button
              type="button"
              class="ghost"
              onClick={() => (spaces.selected.value = null)}
            >
              Show all rooms
            </button>
          </div>
          {spaceScope === 'loading' && (
            <p class="muted">Loading space rooms…</p>
          )}
          {spaceScope === 'failed' && (
            <p class="error" role="alert">
              {spaceScopeError ?? 'Could not load space.'}{' '}
              <button
                type="button"
                class="ghost"
                onClick={() =>
                  selectedSpaceRoom !== undefined &&
                  spaces.refresh(selectedSpaceRoom)
                }
                disabled={selectedSpaceRoom === undefined}
              >
                Retry
              </button>
            </p>
          )}
        </div>
      )}

      {/* Restored rows are honest about being unconfirmed without behaving
          like a loading state: nothing is dimmed, disabled, or moved, because
          a cached list that flinches on every load is worse than the 1.3 s
          wait it replaces (ADR 0085 phase 2). `aria-live="polite"` lets a
          screen reader hear the update settle without interrupting. */}
      {rooms.stale.value && (
        <p class="room-list-stale muted" aria-live="polite">
          Updating…
        </p>
      )}

      {rooms.loading.value ? (
        <p>Loading rooms…</p>
      ) : allRooms.length === 0 ? (
        <p>
          No rooms yet. Rooms appear once an account is added and its sync has
          stored events.
        </p>
      ) : visible.length === 0 ? (
        spaceScope === 'loading' || spaceScope === 'failed' ? null : (
          <p>No rooms match the current filter.</p>
        )
      ) : (
        <ul
          class="room-list"
          ref={list}
          onKeyDown={onListKeyDown}
          style={{ paddingTop: `${padTop}px`, paddingBottom: `${padBottom}px` }}
        >
          {/* Rows are keyed siblings of one array. Wrapping each in a fragment
              to host the separator put the key on the fragment instead, so
              Preact matched rows by position and rebuilt the whole list on
              every re-sort rather than moving the nodes it already had. */}
          {visible.slice(start, end).map((room, offset) => {
            const index = start + offset
            return (
              <RoomRow
                key={roomKey(room)}
                room={room}
                index={index}
                // The rule between two rows belongs to the lower one. The first
                // room has the list's own border above it, and the first
                // unpinned room has the separator.
                rule={index > 0 && index !== pinnedCount}
                accountLabel={
                  showAccountFilter && accountFilter === 'all'
                    ? labels.get(room.account_id)
                    : undefined
                }
                onNavigate={navigateToRoom}
              />
            )
          })}
          {/* Out of flow, so every row keeps the same pitch no matter where the
              pinned section ends — the windowing math depends on that. */}
          {pinnedCount > 0 && pinnedCount < visible.length && (
            <li
              class="room-separator"
              role="separator"
              aria-label="unpinned rooms"
              style={{ top: `${pinnedCount * pitch}px` }}
            />
          )}
        </ul>
      )}
    </>
  )
}

/** The filtered+sorted rooms, pinned first — the TUI's visible_room_indices. */
function visibleRooms(
  allRooms: RoomDto[],
  context: {
    accountFilter: string
    nameQuery: string | null
    roomFilter: RoomFilter
    roomSort: RoomSort
    pinnedRooms: readonly string[]
    roomTitles: ReadonlyMap<string, string>
    hasUnread: (room: RoomDto) => boolean
  },
): RoomDto[] {
  const {
    accountFilter,
    nameQuery,
    roomFilter,
    roomSort,
    pinnedRooms,
    roomTitles,
    hasUnread,
  } = context
  const title = (room: RoomDto) => roomTitle(room, roomTitles)
  const filter: ActiveFilter =
    nameQuery !== null
      ? { kind: 'name', query: nameQuery }
      : { kind: roomFilter }

  const scoped =
    accountFilter === 'all'
      ? allRooms
      : allRooms.filter((room) => room.account_id === accountFilter)
  // `unreadKeys` rather than per-room counts: this runs in the list's own
  // render, and subscribing it to every room's count would put a full
  // filter+sort behind every incoming message. The set changes only when a
  // room crosses 0 ↔ unread, which is exactly when this filter's output can.
  const filtered = filterRooms(scoped, filter, {
    hasUnread: (_key, room) => hasUnread(room),
    isPinned: (key) => pinnedRooms.includes(key),
    title,
  })
  return sortRooms(filtered, pinnedRooms, roomSort, title)
}

/**
 * One row: the name on its own line above a muted account/activity line. The
 * two were side by side until the sidebar shrank them — a nowrap `.room-meta`
 * cannot give up any width, so the name absorbed the whole shortfall and was
 * clipped to nothing once a second account put a user id in the meta.
 */
function RoomRow({
  room,
  accountLabel,
  index,
  rule,
  onNavigate,
}: {
  room: RoomDto
  /** The account to attribute this row to, or `undefined` to omit it. */
  accountLabel?: string
  /** Position in the full filtered list — the handle arrow-key roving uses. */
  index: number
  /** Draw the hairline above this row (see the caller). */
  rule: boolean
  /** Navigate on touch taps, but leave touch scroll gestures to the browser. */
  onNavigate: (room: RoomDto, href: string) => void
}) {
  const { rooms, settings, activeRoom } = useServices()
  const key = roomKey(room)
  const pinned = settings.pinnedRooms.value.includes(key)
  // Reads this room's own count signal, so a count update elsewhere leaves
  // this row alone.
  const count = rooms.unreadCount(key)
  const title = roomTitle(room, rooms.titles.value)
  const href = `/${room.account_id}/rooms/${encodeURIComponent(room.room_id)}`
  const suppressNextClick = useRef(false)
  const suppressClickTimer = useRef<number | null>(null)
  const touchTap = useRef<{
    pointerId: number
    startX: number
    startY: number
    moved: boolean
  } | null>(null)
  const previewEnabled = settings.previewRoom.value
  const preview = previewEnabled ? rooms.preview(key) : undefined
  /** The room the user is reading. `aria-current` is both the state and the CSS hook. */
  const open = activeRoom.value === key

  const suppressSyntheticClick = () => {
    suppressNextClick.current = true
    if (suppressClickTimer.current !== null) {
      window.clearTimeout(suppressClickTimer.current)
    }
    suppressClickTimer.current = window.setTimeout(() => {
      suppressNextClick.current = false
      suppressClickTimer.current = null
    }, 700)
  }

  const clearSyntheticClickSuppression = () => {
    suppressNextClick.current = false
    if (suppressClickTimer.current !== null) {
      window.clearTimeout(suppressClickTimer.current)
      suppressClickTimer.current = null
    }
  }

  useEffect(
    () => () => {
      if (suppressClickTimer.current !== null) {
        window.clearTimeout(suppressClickTimer.current)
      }
    },
    [],
  )

  useEffect(() => {
    if (previewEnabled) {
      perfMark('room-row:hydrate-preview', { key })
      rooms.hydratePreview(room)
    }
  }, [key, previewEnabled, rooms, room])
  const showPreview = previewEnabled && preview !== undefined

  return (
    <li class={rule ? 'room-row rule' : 'room-row'}>
      <a
        href={href}
        class="room-link"
        data-index={index}
        aria-current={open ? 'page' : undefined}
        onPointerDown={(pointerEvent) => {
          if (pointerEvent.pointerType === 'mouse') {
            return
          }
          touchTap.current = {
            pointerId: pointerEvent.pointerId,
            startX: pointerEvent.clientX,
            startY: pointerEvent.clientY,
            moved: false,
          }
        }}
        onPointerMove={(pointerEvent) => {
          const tap = touchTap.current
          if (
            tap === null ||
            tap.pointerId !== pointerEvent.pointerId ||
            tap.moved
          ) {
            return
          }
          const dx = pointerEvent.clientX - tap.startX
          const dy = pointerEvent.clientY - tap.startY
          if (Math.hypot(dx, dy) > TOUCH_TAP_SLOP_PX) {
            tap.moved = true
            suppressSyntheticClick()
          }
        }}
        onPointerCancel={() => {
          touchTap.current = null
          suppressSyntheticClick()
        }}
        onPointerUp={(pointerEvent) => {
          const tap = touchTap.current
          touchTap.current = null
          if (
            pointerEvent.pointerType === 'mouse' ||
            tap === null ||
            tap.pointerId !== pointerEvent.pointerId
          ) {
            return
          }
          const dx = pointerEvent.clientX - tap.startX
          const dy = pointerEvent.clientY - tap.startY
          if (tap.moved || Math.hypot(dx, dy) > TOUCH_TAP_SLOP_PX) {
            suppressSyntheticClick()
            return
          }
          pointerEvent.preventDefault()
          pointerEvent.stopPropagation()
          suppressSyntheticClick()
          onNavigate(room, href)
        }}
        onClickCapture={(click) => {
          if (!suppressNextClick.current) {
            return
          }
          click.preventDefault()
          click.stopPropagation()
          clearSyntheticClickSuppression()
        }}
        onClick={(click) => {
          if (suppressNextClick.current) {
            click.preventDefault()
            clearSyntheticClickSuppression()
            return
          }
          if (!hasModifier(click)) {
            click.preventDefault()
            click.stopPropagation()
            onNavigate(room, href)
            return
          }
          rooms.noteUnreadCounts(room.account_id, room.room_id, 0, 0)
        }}
      >
        <RoomAvatar
          accountId={room.account_id}
          mxcUrl={room.avatar_url ?? null}
          title={title}
          color={roomAvatarColor(key)}
        />
        <span class="room-copy">
          <span class="room-title">
            <span class="room-title-main">
              {/* The name needs its own box: `text-overflow` ellipsizes a block's
                inline content, never a flex item, so the badges cannot share it. */}
              <span class="room-name">{title}</span>
              {isLikelyDm(room) && <span class="badge dm">DM</span>}
              {count > 0 && (
                <span class="badge unread-count" aria-label={`${count} unread`}>
                  {count}
                </span>
              )}
            </span>
            {showPreview && (
              <span class="room-title-meta muted">
                {accountLabel !== undefined && <>{accountLabel} · </>}
                {relativeTime(room.last_activity_ts)}
              </span>
            )}
          </span>
          {showPreview ? (
            <span class="room-preview muted">
              {preview.senderDisplay}: {preview.body}
            </span>
          ) : (
            <span class="room-meta muted">
              {accountLabel !== undefined && <>{accountLabel} · </>}
              {relativeTime(room.last_activity_ts)}
            </span>
          )}
        </span>
      </a>
      <button
        type="button"
        class="ghost pin"
        aria-pressed={pinned}
        title={pinned ? 'Unpin' : 'Pin to top'}
        onClick={() =>
          pinned ? settings.unpinRoom(key) : settings.pinRoom(key)
        }
      >
        {pinned ? '★' : '☆'}
      </button>
    </li>
  )
}

function RoomAvatar({
  accountId,
  mxcUrl,
  title,
  color,
}: {
  accountId: string
  mxcUrl: string | null
  title: string
  color: number
}) {
  const { ref, state } = useMediaBlob<HTMLSpanElement>(accountId, mxcUrl)
  const label = roomAvatarLabel(title)
  return (
    <span
      ref={ref}
      class={`room-avatar room-avatar-color-${color}`}
      aria-hidden="true"
      title={mxcUrl === null ? undefined : `${title} avatar`}
    >
      {state.status === 'ready' && state.url !== undefined ? (
        <img src={state.url} alt="" />
      ) : (
        <span>{label}</span>
      )}
    </span>
  )
}

/** Compact relative timestamp for the activity column. */
function relativeTime(ts: number): string {
  const seconds = Math.round((Date.now() - ts) / 1000)
  if (seconds < 60) {
    return 'now'
  }
  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}m`
  }
  if (seconds < 86400) {
    return `${Math.floor(seconds / 3600)}h`
  }
  if (seconds < 7 * 86400) {
    return `${Math.floor(seconds / 86400)}d`
  }
  return new Date(ts).toISOString().slice(0, 10)
}
