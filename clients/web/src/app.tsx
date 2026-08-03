import { LocationProvider, Route, Router, useLocation } from 'preact-iso'
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from 'preact/hooks'
import { ConnectionIndicator } from './components/ConnectionIndicator'
import { PerfOverlay } from './components/PerfOverlay'
import { RoomList } from './components/RoomList'
import { SpaceList } from './components/SpaceList'
import { SearchOverlay } from './components/SearchOverlay'
import { ShortcutsHelp } from './components/ShortcutsHelp'
import { UnreadThreadsPanel } from './components/UnreadThreadsPanel'
import { UpdateBanner } from './components/UpdateBanner'
import { useModalFocus } from './components/use-modal-focus'
import { layoutMode, SINGLE_PANE_QUERY, useMediaQuery } from './layout'
import {
  localRoomHref,
  parseMatrixRoomReference,
  resolveMatrixToRoomLink,
  type MatrixRoomReference,
} from './matrix-to'
import {
  currentRoomFromPath,
  serializeSearchTokens,
  withSearchParam,
} from './search-tokens'
import { ShellActionsContext } from './shell-actions'
import { AccountsPage } from './pages/AccountsPage'
import { LicensesPage } from './pages/LicensesPage'
import { NotFound } from './pages/NotFound'
import { RoomPage } from './pages/RoomPage'
import { RoomsIndex } from './pages/RoomsIndex'
import { SettingsPage } from './pages/SettingsPage'
import { applyAppBadge } from './app-badge'
import { BUILD_INFO } from './build-info'
import { startAutoRefresh } from './update-refresh'
import { perfEnabled, perfMark, perfMarkFrames, setPerfEnabled } from './perf'
import { setupInstallPromptCapture } from './install-prompt'
import { SLASH_COMMAND } from './slash-commands'
import {
  createServices,
  ServicesContext,
  useServices,
  type AppServices,
} from './services'
import {
  hint,
  isApplePlatform,
  keyAria,
  keyLabel,
  KEYS,
  SHOW_HELP_EVENT,
  useShortcuts,
} from './shortcuts'
import {
  applyTheme,
  SIDEBAR_WIDTH_MAX,
  SIDEBAR_WIDTH_MIN,
} from './stores/settings'
import { roomKey } from './stores/room-list'
import { orderedSpaces } from './stores/spaces'
import type { Account } from './stores/accounts'
import type { RoomEntryResult, RoomsStore } from './stores/rooms'

interface PendingMatrixJoin {
  accountId: string
  accountUserId: string | null
  reference: MatrixRoomReference
  joining: boolean
  error: string | null
}

/**
 * App root (ADR 0046, M-W3). History routing — signed off under ADR 0046
 * open question 5 — with the URL shape that is the deep-link contract:
 * `/:accountId/rooms/:roomId` plus `?thread=` / `?event=` query params.
 * A Tauri-conditional hash fallback is M-W12's problem if the `file://`
 * WebView needs one.
 *
 * `services` is injectable for tests; the app builds the real graph once.
 */
export function App({ services }: { services?: AppServices }) {
  // eslint-disable-next-line react-hooks/exhaustive-deps -- build the real graph exactly once
  const svc = useMemo(() => services ?? createServices(), [])

  useEffect(() => applyTheme(svc.settings, document.documentElement), [svc])
  useEffect(() => applyAppBadge(svc.settings, svc.rooms), [svc])
  // The stored preference drives instrumentation; `?perf=1` still wins for a
  // single session, since `perfEnabled` latches it before this runs.
  useEffect(() => {
    if (!perfEnabled()) {
      setPerfEnabled(svc.settings.perfMarks.value)
    }
  }, [svc, svc.settings.perfMarks.value])
  useVisualViewportShell()
  useStandaloneKeyboardAccessoryInset()
  useInstallPromptCapture()

  // Notice a new build and, when it costs the user nothing, apply it (ADR
  // 0087). Runs signed out too: the sign-in screen is as capable of being a
  // stale bundle as any other, and the manifest needs no auth.
  //
  // Never under `vite dev`, though. HMR already owns reloading there, and the
  // dev stamp is not a deployment identity — it is a git hash plus a `-dirty`
  // flag read once at server start, so it moves whenever the working tree does.
  // Restart the dev server after a commit or an edit and every open tab would
  // see a "new build" and reload itself, which is indistinguishable from a bug
  // and fights the HMR update arriving at the same moment.
  useEffect(() => {
    if (import.meta.env.DEV) {
      return
    }
    return startAutoRefresh({
      updates: svc.updates,
      currentVersion: BUILD_INFO.version,
      flush: () => svc.deviceState.flushPending(),
      hasUnsentWork: () => svc.timelines.hasUnsentWork,
    })
  }, [svc])

  // Hold the live socket open only while signed in; sign-out tears it down
  // (M-W6, ADR 0061). Reconnect/backoff on unexpected drops arrives in step 3.
  useEffect(() => {
    if (!svc.auth.signedIn.value) {
      return
    }
    svc.live.start()
    return () => svc.live.stop()
  }, [svc, svc.auth.signedIn.value])

  return (
    <ServicesContext.Provider value={svc}>
      {svc.auth.signedIn.value ? (
        <Shell />
      ) : window.location.pathname === '/oauth/callback' ? (
        <OAuthCallback />
      ) : (
        <SignedOut />
      )}
      <PerfOverlay />
    </ServicesContext.Provider>
  )
}

function useInstallPromptCapture(): void {
  useEffect(() => setupInstallPromptCapture(window), [])
}

function useVisualViewportShell(): void {
  useEffect(() => {
    const root = document.documentElement
    const viewport = window.visualViewport
    const clear = () => {
      root.style.removeProperty('--app-viewport-top')
      root.style.removeProperty('--app-viewport-left')
      root.style.removeProperty('--app-viewport-width')
      root.style.removeProperty('--app-viewport-height')
    }
    if (viewport == null) {
      clear()
      return
    }
    const update = () => {
      // A short visual viewport is only legitimate while the keyboard is up or
      // the page is pinch-zoomed. When neither holds, the reading is stale:
      // iOS does not reliably fire a viewport `resize` when the keyboard goes
      // away *with* the element that had focus — closing a thread panel or
      // following a link unmounts the focused composer, and the dismissal
      // arrives with no event. Pinning the fixed shell to that stale height
      // would leave it covering only the top of the screen with bare
      // background below, so fall back to the CSS `100dvh` sizing and let the
      // next real event take over.
      if (isViewportHeightStale(viewport)) {
        clear()
        return
      }
      root.style.setProperty('--app-viewport-top', `${viewport.offsetTop}px`)
      root.style.setProperty('--app-viewport-left', `${viewport.offsetLeft}px`)
      root.style.setProperty('--app-viewport-width', `${viewport.width}px`)
      root.style.setProperty('--app-viewport-height', `${viewport.height}px`)
    }
    // The keyboard leaves over about a frame, and a blur that comes from the
    // focused node being removed reports the pre-dismissal size, so re-measure
    // once the event loop has drained (the `focusout` idiom used for the
    // accessory inset below).
    const updateAfterBlur = () => setTimeout(update)
    update()
    viewport.addEventListener('resize', update)
    viewport.addEventListener('scroll', update)
    window.addEventListener('orientationchange', update)
    window.addEventListener('resize', update)
    window.addEventListener('pageshow', update)
    document.addEventListener('focusin', update)
    document.addEventListener('focusout', updateAfterBlur)
    return () => {
      viewport.removeEventListener('resize', update)
      viewport.removeEventListener('scroll', update)
      window.removeEventListener('orientationchange', update)
      window.removeEventListener('resize', update)
      window.removeEventListener('pageshow', update)
      document.removeEventListener('focusin', update)
      document.removeEventListener('focusout', updateAfterBlur)
      clear()
    }
  }, [])
}

/** How far under the layout viewport counts as a shrink, not rounding (px). */
const VIEWPORT_SHRINK_EPSILON = 1
/** How far `scale` may drift from 1 and still count as un-zoomed. */
const VIEWPORT_ZOOM_EPSILON = 0.01

/**
 * Whether `viewport.height` is shorter than the window for no reason we can
 * see — no editable focused (so no keyboard) and no pinch zoom. Browsers that
 * do not report `scale` are left alone: without it a zoom cannot be told from
 * a stale reading, and the event listeners still correct the common cases.
 */
function isViewportHeightStale(viewport: VisualViewport): boolean {
  return (
    !isEditableFocused() &&
    typeof viewport.scale === 'number' &&
    viewport.scale <= 1 + VIEWPORT_ZOOM_EPSILON &&
    viewport.height < window.innerHeight - VIEWPORT_SHRINK_EPSILON
  )
}

/** Whether focus sits in a text field — the app's proxy for "keyboard up". */
function isEditableFocused(): boolean {
  const active = document.activeElement
  return (
    active instanceof HTMLElement &&
    (active.matches('textarea, input') ||
      active.getAttribute('contenteditable') === 'true')
  )
}

function useStandaloneKeyboardAccessoryInset(): void {
  useEffect(() => {
    const root = document.documentElement
    const standaloneMedia = window.matchMedia?.('(display-mode: standalone)')
    const clear = () => {
      root.style.removeProperty('--app-keyboard-accessory-inset')
      root.style.removeProperty('--app-standalone-composer-bottom-padding')
    }
    const update = () => {
      const editableFocused = isEditableFocused()
      if (isApplePlatform() && isStandaloneDisplay(standaloneMedia)) {
        root.style.setProperty(
          '--app-standalone-composer-bottom-padding',
          '4px',
        )
        if (editableFocused) {
          root.style.setProperty('--app-keyboard-accessory-inset', '4px')
        } else {
          root.style.removeProperty('--app-keyboard-accessory-inset')
        }
      } else {
        clear()
      }
    }

    update()
    document.addEventListener('focusin', update)
    const updateAfterBlur = () => setTimeout(update)
    document.addEventListener('focusout', updateAfterBlur)
    standaloneMedia?.addEventListener?.('change', update)
    return () => {
      document.removeEventListener('focusin', update)
      document.removeEventListener('focusout', updateAfterBlur)
      standaloneMedia?.removeEventListener?.('change', update)
      clear()
    }
  }, [])
}

function isStandaloneDisplay(media: MediaQueryList | undefined): boolean {
  const navigatorStandalone = (
    navigator as Navigator & { standalone?: boolean }
  ).standalone
  return navigatorStandalone === true || media?.matches === true
}

function isReloadOrRestoreNavigation(): boolean {
  const [entry] = performance.getEntriesByType(
    'navigation',
  ) as PerformanceNavigationTiming[]
  if (entry !== undefined) {
    return entry.type === 'reload' || entry.type === 'back_forward'
  }
  const legacyNavigation = (
    performance as Performance & {
      navigation?: {
        type: number
        TYPE_RELOAD: number
        TYPE_BACK_FORWARD: number
      }
    }
  ).navigation
  return (
    legacyNavigation !== undefined &&
    (legacyNavigation.type === legacyNavigation.TYPE_RELOAD ||
      legacyNavigation.type === legacyNavigation.TYPE_BACK_FORWARD)
  )
}

/** The signed-out state: the auth provider's bootstrap UI. */
function SignedOut() {
  const { auth } = useServices()
  return (
    <main class="signin">
      <h1>axon</h1>
      <p>Sign in with SSO or a server-issued access token.</p>
      <auth.LoginBootstrap />
    </main>
  )
}

function OAuthCallback() {
  const { auth } = useServices()
  const [message, setMessage] = useState('Completing sign-in...')
  const [failed, setFailed] = useState(false)

  useEffect(() => {
    let cancelled = false
    void auth
      .completeOAuthRedirect(new URL(window.location.href))
      .then((result) => {
        if (cancelled || result.ok) {
          return
        }
        setFailed(true)
        setMessage(result.message)
      })
    return () => {
      cancelled = true
    }
  }, [auth])

  return (
    <main class="signin">
      <h1>axon</h1>
      <p class={failed ? 'error' : 'muted'}>{message}</p>
      {failed && <a href="/">Back to sign in</a>}
    </main>
  )
}

/** The signed-in shell. `LocationProvider` wraps the chrome, not the reverse. */
function Shell() {
  return (
    <LocationProvider>
      <ShellChrome />
    </LocationProvider>
  )
}

const MIN_TIMELINE_WIDTH = 320

function availableSidebarWidthMaximum(): number {
  return Math.max(
    SIDEBAR_WIDTH_MIN,
    Math.min(SIDEBAR_WIDTH_MAX, window.innerWidth - MIN_TIMELINE_WIDTH),
  )
}

function clampSidebarWidth(width: number): number {
  return Math.round(
    Math.max(
      SIDEBAR_WIDTH_MIN,
      Math.min(availableSidebarWidthMaximum(), width),
    ),
  )
}

/** One pane-edge control: click to collapse, drag or arrow-key to resize. */
function SidebarPaneHandle({
  width,
  onResize,
  collapsed,
  onToggle,
}: {
  width: number
  onResize: (width: number) => void
  collapsed: boolean
  onToggle: () => void
}) {
  const drag = useRef<{
    startX: number
    startWidth: number
    moved: boolean
  } | null>(null)
  const suppressClick = useRef(false)
  const maximum = availableSidebarWidthMaximum()

  return (
    <button
      type="button"
      class="pane-collapse-tab sidebar-collapse-tab"
      aria-expanded={!collapsed}
      aria-controls="room-sidebar"
      aria-label={collapsed ? 'Show rooms' : 'Hide rooms'}
      title={`${hint(
        collapsed ? 'Show rooms' : 'Hide rooms',
        KEYS.toggleSidebar,
      )}${collapsed ? '' : '; drag or use arrow keys to resize'}`}
      aria-keyshortcuts={keyAria(KEYS.toggleSidebar)}
      onPointerDown={(event) => {
        if (event.button !== 0 || collapsed) return
        drag.current = {
          startX: event.clientX,
          startWidth: width,
          moved: false,
        }
        event.currentTarget.setPointerCapture(event.pointerId)
      }}
      onPointerMove={(event) => {
        const start = drag.current
        if (start === null) return
        const distance = event.clientX - start.startX
        if (Math.abs(distance) > 2) start.moved = true
        onResize(clampSidebarWidth(start.startWidth + distance))
      }}
      onPointerUp={(event) => {
        const activeDrag = drag.current
        suppressClick.current = activeDrag?.moved ?? false
        drag.current = null
        if (activeDrag !== null) {
          event.currentTarget.releasePointerCapture(event.pointerId)
        }
      }}
      onPointerCancel={(event) => {
        const activeDrag = drag.current
        drag.current = null
        if (activeDrag !== null) {
          event.currentTarget.releasePointerCapture(event.pointerId)
        }
      }}
      onKeyDown={(event) => {
        if (collapsed) return
        let next: number | null = null
        if (event.key === 'ArrowLeft') next = width - 16
        if (event.key === 'ArrowRight') next = width + 16
        if (event.key === 'Home') next = SIDEBAR_WIDTH_MIN
        if (event.key === 'End') next = maximum
        if (next === null) return
        event.preventDefault()
        onResize(clampSidebarWidth(next))
      }}
      onClick={() => {
        if (suppressClick.current) {
          suppressClick.current = false
          return
        }
        onToggle()
      }}
    >
      <span aria-hidden="true">{collapsed ? '›' : '‹'}</span>
    </button>
  )
}

/**
 * Header plus the two panes (ADR 0062). Separate from `Shell` because
 * `useLocation` reads the context `Shell` itself only *renders* — a hook call
 * up there would see no provider — and both the topbar and the panes need the
 * current layout mode.
 *
 * The sidebar stays mounted in every mode and is hidden with CSS. Unmounting
 * it on a room switch would throw away its scroll position and the room list's
 * session-only name/account filters.
 */
function ShellChrome() {
  const location = useLocation()
  const { path, query } = location
  const { accounts, rooms, search, settings, spaces, threadUnread } =
    useServices()
  const mode = layoutMode(path)
  perfMark('shell:render', { path, mode })
  const collapsed = settings.sidebarCollapsed.value
  const sidebarWidth = settings.sidebarWidth.value
  const [helpOpen, setHelpOpen] = useState(false)
  const [unreadThreadsOpen, setUnreadThreadsOpen] = useState(false)
  const [jumpAction, setJumpActionState] = useState<(() => void) | null>(null)
  const [roomLinkJoinError, setRoomLinkJoinError] = useState<string | null>(
    null,
  )
  const [pendingMatrixJoin, setPendingMatrixJoin] =
    useState<PendingMatrixJoin | null>(null)
  const [roomChrome, setRoomChromeState] = useState<{
    title: string | null
    action: (() => void) | null
  }>({ title: null, action: null })
  const singlePane = useMediaQuery(SINGLE_PANE_QUERY)
  // The search overlay is URL-addressed (ADR 0066): mounted while `?search=`
  // is present (even empty), so a shared link restores it and Back closes it.
  const searchOpen = typeof query.search === 'string'
  const accountCount = accounts.accounts.value.length
  const accountsLoading = accounts.loading.value
  const accountsError = accounts.error.value
  const roomListLoading = rooms.loading.value
  const roomEntries = rooms.rooms.value
  const roomTitles = rooms.titles.value
  const joinedSpaces = useMemo(
    () => roomEntries.filter((room) => room.room_type === 'm.space'),
    [roomEntries],
  )
  const hasSpaces = joinedSpaces.length > 0
  // Auto-hiding a lone space is tracked separately from a deliberate collapse:
  // single-pane widths have no toggle at all, so a persisted manual collapse
  // would otherwise be unrecoverable there (see index.css).
  const spacesPaneAutoHidden =
    !settings.spacesPaneCollapsed.value &&
    settings.spacesPaneAutoHide.value &&
    joinedSpaces.length <= 1
  const spacesPaneCollapsed =
    settings.spacesPaneCollapsed.value || spacesPaneAutoHidden
  const unreadThreadCount = threadUnread.count.value
  const roomTitleButton = useRef<HTMLButtonElement>(null)
  const startupThreadScrubbed = useRef(false)
  const inboundMatrixHandled = useRef<string | null>(null)
  const priorRoom = useRef(currentRoomFromPath(path))

  useEffect(() => {
    const current = currentRoomFromPath(path)
    const previous = priorRoom.current
    if (previous === null && current !== null) {
      // A search result can be opened from settings, accounts, or the room
      // list. Consume its one-shot preservation here so it cannot leak into a
      // later unrelated room switch.
      search.consumeResultJumpPreservation()
    }
    if (
      previous !== null &&
      (current === null ||
        current.accountId !== previous.accountId ||
        current.roomId !== previous.roomId)
    ) {
      if (!search.consumeResultJumpPreservation()) {
        search.clear()
      }
    }
    priorRoom.current = current
  }, [path, search])

  const openHelp = (event: KeyboardEvent) => {
    event.preventDefault()
    setHelpOpen(true)
  }

  const openSearch = (event?: KeyboardEvent) => {
    event?.preventDefault()
    if (!searchOpen) {
      const lastQuery = search.lastQuery.value
      location.route(
        withSearchParam(
          location.url,
          lastQuery === null ? '' : serializeSearchTokens(lastQuery),
        ),
      )
    }
  }
  const toggleSpacesPane = useCallback(() => {
    settings.spacesPaneAutoHide.value = false
    settings.spacesPaneCollapsed.value = !spacesPaneCollapsed
  }, [settings, spacesPaneCollapsed])
  const stepSpace = useCallback(
    (direction: -1 | 1) => {
      const ordered = orderedSpaces(
        rooms.rooms.value.filter((room) => room.room_type === 'm.space'),
        settings.spaceOrder.value,
        rooms.titles.value,
      )
      if (ordered.length === 0) return
      const current = spaces.selected.value
      const index = ordered.findIndex((room) => roomKey(room) === current)
      const next =
        index === -1
          ? direction === 1
            ? 0
            : ordered.length - 1
          : (index + direction + ordered.length) % ordered.length
      settings.spacesPaneAutoHide.value = false
      settings.spacesPaneCollapsed.value = false
      spaces.selected.value = roomKey(ordered[next])
    },
    [rooms, settings, spaces],
  )
  const setJumpAction = useCallback((action: (() => void) | null) => {
    setJumpActionState(() => action)
  }, [])
  const setRoomChrome = useCallback(
    (title: string | null, action: (() => void) | null) => {
      setRoomChromeState({ title, action })
    },
    [],
  )
  const shellActions = useMemo(
    () => ({
      jumpAction,
      setJumpAction,
      openUnreadThreads: () => setUnreadThreadsOpen(true),
      roomTitle: roomChrome.title,
      roomInfoAction: roomChrome.action,
      setRoomChrome,
    }),
    [jumpAction, roomChrome, setJumpAction, setRoomChrome],
  )
  const mobileRoomChrome = mode === 'room' && singlePane

  useLayoutEffect(() => {
    perfMark('shell:layout-effect', { path, mode })
    perfMarkFrames('shell:post-layout')
    if (startupThreadScrubbed.current) {
      return
    }
    startupThreadScrubbed.current = true
    if (typeof query.thread !== 'string' || !isReloadOrRestoreNavigation()) {
      return
    }
    const params = new URLSearchParams(window.location.search)
    params.delete('thread')
    const nextQuery = params.toString()
    location.route(`${path}${nextQuery === '' ? '' : `?${nextQuery}`}`, true)
  }, [location, mode, path, query.thread])

  useEffect(() => {
    if (mobileRoomChrome) {
      roomTitleButton.current?.focus()
    }
  }, [mobileRoomChrome, path])

  useEffect(() => {
    const onShowHelp = () => setHelpOpen(true)
    window.addEventListener(SHOW_HELP_EVENT, onShowHelp)
    return () => window.removeEventListener(SHOW_HELP_EVENT, onShowHelp)
  }, [])
  useEffect(() => {
    if (accountsLoading) {
      void accounts.refresh()
    }
  }, [accounts, accountsLoading])
  useEffect(() => {
    if (
      !accountsLoading &&
      accountsError === null &&
      accountCount === 0 &&
      path !== '/accounts'
    ) {
      location.route('/accounts', true)
    }
  }, [accountCount, accountsError, accountsLoading, location, path])
  useEffect(() => {
    const raw =
      typeof query.matrixLink === 'string'
        ? query.matrixLink
        : typeof query.matrix === 'string'
          ? query.matrix
          : null
    if (
      raw === null ||
      raw === inboundMatrixHandled.current ||
      accountsLoading ||
      accountsError !== null ||
      roomListLoading
    ) {
      return
    }
    const accountId = accountIdForRoomEntry(
      path,
      accounts.accounts.value,
      settings.activeAccountId.value,
    )
    if (accountId === null) {
      return
    }
    inboundMatrixHandled.current = raw
    const reference = parseMatrixRoomReference(raw)
    if (reference === null) {
      scrubMatrixQuery(location)
      return
    }
    const resolved = resolveMatrixToRoomLink(raw, {
      accountId,
      rooms: roomEntries,
      roomTitles,
    })
    if (resolved?.action === 'open') {
      location.route(resolved.href, true)
      return
    }
    const account = accounts.accounts.value.find(
      (candidate) => candidate.account_id === accountId,
    )
    scrubMatrixQuery(location)
    setPendingMatrixJoin({
      accountId,
      accountUserId: account?.user_id ?? null,
      reference,
      joining: false,
      error: null,
    })
  }, [
    accounts.accounts.value,
    accountsError,
    accountsLoading,
    location,
    path,
    query.matrix,
    query.matrixLink,
    roomEntries,
    roomListLoading,
    roomTitles,
    rooms,
    settings.activeAccountId.value,
  ])
  const cancelPendingMatrixJoin = useCallback(() => {
    setPendingMatrixJoin(null)
  }, [])
  const confirmPendingMatrixJoin = useCallback(() => {
    const pending = pendingMatrixJoin
    if (pending === null || pending.joining) {
      return
    }
    setPendingMatrixJoin({ ...pending, joining: true, error: null })
    void rooms
      .joinRoom(
        pending.accountId,
        pending.reference.roomIdOrAlias,
        pending.reference.serverNames,
      )
      .then((result) => {
        if (!result.ok) {
          setPendingMatrixJoin((current) =>
            current?.accountId === pending.accountId &&
            current.reference === pending.reference
              ? { ...current, joining: false, error: result.message }
              : current,
          )
          return
        }
        setPendingMatrixJoin((current) =>
          current?.accountId === pending.accountId &&
          current.reference === pending.reference
            ? null
            : current,
        )
        location.route(
          localRoomHref(
            pending.accountId,
            result.roomId,
            pending.reference.eventId,
          ),
        )
      })
  }, [location, pendingMatrixJoin, rooms])
  useEffect(() => {
    const onClick = (event: MouseEvent) => {
      if (
        event.defaultPrevented ||
        event.button !== 0 ||
        event.ctrlKey ||
        event.metaKey ||
        event.altKey ||
        event.shiftKey
      ) {
        return
      }
      const anchor = (event.target as Element | null)?.closest?.('a[href]')
      if (!(anchor instanceof HTMLAnchorElement)) {
        return
      }
      if (anchor.download !== '') {
        return
      }
      const reference = parseMatrixRoomReference(anchor.href)
      if (reference === null) {
        return
      }
      const accountId = accountIdForRoomEntry(
        path,
        accounts.accounts.value,
        settings.activeAccountId.value,
      )
      if (accountId === null) {
        return
      }
      event.preventDefault()
      setRoomLinkJoinError(null)
      void joinMatrixRoomReference(location, rooms, accountId, reference).then(
        (result) => {
          if (!result.ok) {
            setRoomLinkJoinError(
              `Could not join ${reference.roomIdOrAlias}: ${result.message}`,
            )
          }
        },
      )
    }
    document.addEventListener('click', onClick)
    return () => document.removeEventListener('click', onClick)
  }, [
    accounts.accounts.value,
    location,
    path,
    rooms,
    settings.activeAccountId.value,
  ])

  useShortcuts({
    // Bare characters, so `useShortcuts` already withholds them while typing.
    '?': openHelp,
    '/': openSearch,
  })
  useShortcuts(
    {
      // Search's modifier twin (ADR 0066), reachable from the composer.
      'mod+shift+f': (event) => {
        if (isApplePlatform()) {
          return
        }
        openSearch(event)
      },
      'mod+g': (event) => {
        if (!isApplePlatform()) {
          return
        }
        openSearch(event)
      },
      'mod+b': (event) => {
        if (mode === 'utility' || singlePane) {
          return // no sidebar here to collapse
        }
        event.preventDefault()
        settings.sidebarCollapsed.value = !collapsed
      },
      'mod+alt+s': (event) => {
        if (mode === 'utility' || singlePane || !hasSpaces) return
        event.preventDefault()
        toggleSpacesPane()
      },
      'mod+alt+r': (event) => {
        // Unlike the other space chords this one is useful in single-pane mode,
        // where the picker is a horizontal strip and drag-and-drop is absent.
        if (mode === 'utility' || !hasSpaces) return
        event.preventDefault()
        spaces.reordering.value = !spaces.reordering.value
      },
      'mod+alt+[': (event) => {
        if (mode === 'utility' || singlePane || !hasSpaces) return
        event.preventDefault()
        stepSpace(-1)
      },
      'mod+alt+]': (event) => {
        if (mode === 'utility' || singlePane || !hasSpaces) return
        event.preventDefault()
        stepSpace(1)
      },
      'mod+alt+m': (event) => {
        event.preventDefault()
        location.route('/rooms/dm')
      },
      Escape: (event) => {
        if (path !== '/settings') {
          return
        }
        event.preventDefault()
        location.route('/')
      },
      // Help must be reachable from the composer, where focus usually is, and
      // `?` never can be — it has to reach the textarea as a character.
      // Ctrl+Shift+/ reports `event.key === '?'` on a US layout and `/` on
      // others, so accept every spelling rather than guess the layout.
      'mod+/': openHelp,
      'mod+shift+/': openHelp,
      'mod+shift+?': openHelp,
    },
    { whileTyping: true },
  )

  return (
    <ShellActionsContext.Provider value={shellActions}>
      <div class={`shell${mobileRoomChrome ? ' mobile-room-shell' : ''}`}>
        <header class="topbar">
          <div class="topbar-brand-lockup">
            <a href="/" class="brand topbar-brand">
              axon
            </a>
            <ConnectionIndicator />
          </div>
          {mobileRoomChrome && (
            <a
              href="/"
              class="ghost topbar-icon-button topbar-rooms-button"
              aria-label="Rooms"
              title="Rooms"
            >
              <MenuIcon />
            </a>
          )}
          {mobileRoomChrome && (
            <button
              ref={roomTitleButton}
              type="button"
              class="topbar-room-title"
              aria-label="Open room information"
              title={`Room information (${SLASH_COMMAND.whereami})`}
              onClick={() => roomChrome.action?.()}
            >
              <span>{roomChrome.title ?? 'Room'}</span>
            </button>
          )}
          {mode === 'room' && jumpAction !== null && !singlePane && (
            <button
              type="button"
              class="ghost"
              aria-haspopup="dialog"
              title={`Jump to a date (${SLASH_COMMAND.jump})`}
              onClick={jumpAction}
            >
              Jump
            </button>
          )}
          <div class="topbar-actions">
            <button
              type="button"
              class="ghost topbar-icon-button unread-threads-button"
              title={`Unread threads (${SLASH_COMMAND.unreadthreads})`}
              aria-label={
                unreadThreadCount === 0
                  ? 'Unread threads'
                  : `Unread threads, ${unreadThreadCount}`
              }
              aria-haspopup="dialog"
              onClick={() => setUnreadThreadsOpen(true)}
            >
              <ThreadIcon />
              {unreadThreadCount > 0 && (
                <span class="topbar-count-badge">{unreadThreadCount}</span>
              )}
              <span class="topbar-label">Threads</span>
            </button>
            <button
              type="button"
              class="ghost topbar-icon-button"
              title={`Search messages (${SLASH_COMMAND.search}; ${keyLabel(KEYS.search)})`}
              aria-label="Search messages"
              aria-keyshortcuts={keyAria(KEYS.search)}
              aria-haspopup="dialog"
              onClick={() => openSearch()}
            >
              <SearchIcon />
              <span class="topbar-label">Search</span>
            </button>
            <a
              href="/settings"
              class="ghost topbar-icon-button"
              title="Settings"
              aria-label="Settings"
            >
              <SettingsIcon />
              <span class="topbar-label">Settings</span>
            </a>
            {/* Keyboard-free discovery: nothing else tells you the chords exist. */}
            <button
              type="button"
              class="ghost help-button topbar-icon-button"
              title={`Keyboard shortcuts (${SLASH_COMMAND.help}; ${keyLabel(KEYS.showHelp)})`}
              aria-label="Keyboard shortcuts"
              aria-keyshortcuts={keyAria(KEYS.showHelp)}
              aria-haspopup="dialog"
              onClick={() => setHelpOpen(true)}
            >
              ?
            </button>
          </div>
        </header>

        <UpdateBanner />

        {roomLinkJoinError !== null && (
          <div class="banner error shell-banner" role="alert">
            <span>{roomLinkJoinError}</span>
            <button
              type="button"
              class="ghost"
              onClick={() => setRoomLinkJoinError(null)}
            >
              Dismiss
            </button>
          </div>
        )}

        <div
          class={`shell-body mode-${mode}${collapsed ? ' sidebar-collapsed' : ''}${spacesPaneCollapsed ? ' spaces-pane-collapsed' : ''}${spacesPaneAutoHidden ? ' spaces-pane-auto-hidden' : ''}`}
        >
          <nav
            id="room-sidebar"
            class="sidebar"
            aria-label="Rooms"
            style={{ flexBasis: `${sidebarWidth}px` }}
          >
            {hasSpaces && (
              <>
                <SpaceList />
                <button
                  type="button"
                  class="pane-collapse-tab space-pane-collapse-tab"
                  aria-expanded={!spacesPaneCollapsed}
                  aria-controls="spaces-pane"
                  aria-label={
                    spacesPaneCollapsed ? 'Show spaces' : 'Hide spaces'
                  }
                  title={hint(
                    spacesPaneCollapsed ? 'Show spaces' : 'Hide spaces',
                    KEYS.toggleSpaces,
                  )}
                  aria-keyshortcuts={keyAria(KEYS.toggleSpaces)}
                  onClick={toggleSpacesPane}
                >
                  <span aria-hidden="true">
                    {spacesPaneCollapsed ? '›' : '‹'}
                  </span>
                </button>
              </>
            )}
            <div class="room-list-pane">
              <RoomList />
            </div>
          </nav>
          {mode !== 'utility' && !singlePane && (
            <SidebarPaneHandle
              width={sidebarWidth}
              collapsed={collapsed}
              onToggle={() => (settings.sidebarCollapsed.value = !collapsed)}
              onResize={(width) => (settings.sidebarWidth.value = width)}
            />
          )}
          <main>
            <Router>
              <Route path="/" component={RoomsIndex} />
              <Route path="/rooms/discover" component={RoomsIndex} />
              <Route path="/rooms/create" component={RoomsIndex} />
              <Route path="/rooms/dm" component={RoomsIndex} />
              <Route path="/accounts" component={AccountsPage} />
              <Route path="/settings" component={SettingsPage} />
              <Route path="/licenses" component={LicensesPage} />
              <Route path="/:accountId/rooms/:roomId" component={RoomPage} />
              <Route default component={NotFound} />
            </Router>
          </main>
        </div>

        {searchOpen && <SearchOverlay />}
        {unreadThreadsOpen && (
          <UnreadThreadsPanel onClose={() => setUnreadThreadsOpen(false)} />
        )}
        {pendingMatrixJoin !== null && (
          <MatrixJoinPrompt
            pending={pendingMatrixJoin}
            onCancel={cancelPendingMatrixJoin}
            onJoin={confirmPendingMatrixJoin}
          />
        )}
        {helpOpen && (
          <ShortcutsHelp
            mobile={singlePane}
            onClose={() => setHelpOpen(false)}
          />
        )}
      </div>
    </ShellActionsContext.Provider>
  )
}

function MatrixJoinPrompt({
  pending,
  onCancel,
  onJoin,
}: {
  pending: PendingMatrixJoin
  onCancel: () => void
  onJoin: () => void
}) {
  const { containerRef } = useModalFocus<HTMLDivElement>()
  useShortcuts(
    {
      Escape: (event) => {
        event.preventDefault()
        if (!pending.joining) {
          onCancel()
        }
      },
    },
    { whileTyping: true, capture: true },
  )

  return (
    <div
      ref={containerRef}
      class="overlay"
      role="dialog"
      aria-modal="true"
      aria-labelledby="matrix-join-title"
    >
      <section class="overlay-panel matrix-join-dialog">
        <div class="overlay-head">
          <h2 id="matrix-join-title">Join room?</h2>
          <button
            type="button"
            class="ghost"
            disabled={pending.joining}
            onClick={onCancel}
          >
            Close
          </button>
        </div>
        <p>
          Another app or link requested that Axon join{' '}
          <code>{pending.reference.roomIdOrAlias}</code>.
        </p>
        {pending.accountUserId !== null && (
          <p class="muted">Account: {pending.accountUserId}</p>
        )}
        {pending.error !== null && <p class="error">{pending.error}</p>}
        <div class="dialog-actions">
          <button
            type="button"
            class="ghost"
            disabled={pending.joining}
            onClick={onCancel}
          >
            Cancel
          </button>
          <button type="button" disabled={pending.joining} onClick={onJoin}>
            {pending.joining ? 'Joining...' : 'Join'}
          </button>
        </div>
      </section>
    </div>
  )
}

export function accountIdForRoomEntry(
  path: string,
  accounts: readonly Account[],
  activeAccountId: string | null,
): string | null {
  const route = /^\/([^/]+)\/rooms\//.exec(path)
  if (route !== null) {
    return safeDecodeURIComponent(route[1])
  }
  const active = accounts.find(
    (account) =>
      account.state === 'active' && account.account_id === activeAccountId,
  )
  if (active !== undefined) {
    return active.account_id
  }
  return (
    accounts.find((account) => account.state === 'active')?.account_id ?? null
  )
}

function safeDecodeURIComponent(value: string): string | null {
  try {
    return decodeURIComponent(value)
  } catch {
    return null
  }
}

async function joinMatrixRoomReference(
  location: ReturnType<typeof useLocation>,
  rooms: RoomsStore,
  accountId: string,
  reference: MatrixRoomReference,
): Promise<RoomEntryResult> {
  const result = await rooms.joinRoom(
    accountId,
    reference.roomIdOrAlias,
    reference.serverNames,
  )
  if (!result.ok) {
    return result
  }
  location.route(localRoomHref(accountId, result.roomId, reference.eventId))
  return result
}

function scrubMatrixQuery(location: ReturnType<typeof useLocation>): void {
  const params = new URLSearchParams(window.location.search)
  params.delete('matrix')
  params.delete('matrixLink')
  const nextQuery = params.toString()
  location.route(
    `${location.path}${nextQuery === '' ? '' : `?${nextQuery}`}`,
    true,
  )
}

function ThreadIcon() {
  return (
    <svg class="topbar-icon" viewBox="0 0 24 24" aria-hidden="true">
      <path
        d="M21 12a8 8 0 0 1-8 8H7l-4 3v-7a8 8 0 1 1 18-4Z"
        fill="none"
        stroke="currentColor"
        stroke-linecap="round"
        stroke-linejoin="round"
        stroke-width="2"
      />
      <path
        d="M8 11h8M8 15h5"
        fill="none"
        stroke="currentColor"
        stroke-linecap="round"
        stroke-width="2"
      />
    </svg>
  )
}

function MenuIcon() {
  return (
    <svg class="topbar-icon" viewBox="0 0 24 24" aria-hidden="true">
      <path
        d="M4 7h16M4 12h16M4 17h16"
        fill="none"
        stroke="currentColor"
        stroke-linecap="round"
        stroke-width="2"
      />
    </svg>
  )
}

function SearchIcon() {
  return (
    <svg class="topbar-icon" viewBox="0 0 24 24" aria-hidden="true">
      <path
        d="m21 21-4.35-4.35M11 18a7 7 0 1 1 0-14 7 7 0 0 1 0 14Z"
        fill="none"
        stroke="currentColor"
        stroke-linecap="round"
        stroke-linejoin="round"
        stroke-width="2"
      />
    </svg>
  )
}

function SettingsIcon() {
  return (
    <svg class="topbar-icon" viewBox="0 0 24 24" aria-hidden="true">
      <path
        d="M12 15.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7Z"
        fill="none"
        stroke="currentColor"
        stroke-width="2"
      />
      <path
        d="M19.4 15a1.8 1.8 0 0 0 .36 1.98l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06A1.8 1.8 0 0 0 15 19.4a1.8 1.8 0 0 0-1 .55V20a2 2 0 1 1-4 0v-.09a1.8 1.8 0 0 0-1-.55 1.8 1.8 0 0 0-1.98.36l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.8 1.8 0 0 0 4.6 15a1.8 1.8 0 0 0-.55-1H4a2 2 0 1 1 0-4h.09a1.8 1.8 0 0 0 .55-1 1.8 1.8 0 0 0-.36-1.98l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.8 1.8 0 0 0 9 4.6a1.8 1.8 0 0 0 1-.55V4a2 2 0 1 1 4 0v.09a1.8 1.8 0 0 0 1 .55 1.8 1.8 0 0 0 1.98-.36l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.8 1.8 0 0 0 19.4 9c.2.34.39.68.55 1H20a2 2 0 1 1 0 4h-.09a1.8 1.8 0 0 0-.55 1Z"
        fill="none"
        stroke="currentColor"
        stroke-linecap="round"
        stroke-linejoin="round"
        stroke-width="2"
      />
    </svg>
  )
}
