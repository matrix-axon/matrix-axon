import { effect, signal, type Signal } from '@preact/signals'
import { createContext } from 'preact'
import { useContext } from 'preact/hooks'
import { createApiClient, type ApiClient } from './api/client'
import {
  deviceStateChange,
  ephemeralPassthrough,
  timelineEvent,
  unreadCountsChange,
} from './api/frames'
import { openLiveSocket } from './api/ws'
import {
  createCompositeAuthProvider,
  type CompositeAuthProvider,
} from './auth/composite'
import { parseOAuthProviders } from './auth/oauth'
import { createAccountsStore, type AccountsStore } from './stores/accounts'
import {
  createDeviceStateStore,
  parseThreadReadMarker,
  READ_MARKERS_NAMESPACE,
  THREAD_READ_MARKERS_NAMESPACE,
  type DeviceStateStore,
} from './stores/device-state'
import { createEphemeralStore, type EphemeralStore } from './stores/ephemeral'
import {
  createEphemeralSender,
  type EphemeralSender,
} from './stores/ephemeral-sender'
import { createMediaService, type MediaService } from './media/media-service'
import {
  createLiveConnection,
  type LiveConnection,
} from './stores/live-connection'
import { roomTitle } from './stores/room-list'
import { createRoomsStore, type RoomsStore } from './stores/rooms'
import { createSearchStore, type SearchStore } from './stores/search'
import { createSettingsStore, type SettingsStore } from './stores/settings'
import {
  createThreadUnreadStore,
  type ActiveThread,
  type ThreadUnreadStore,
} from './stores/thread-unread'
import {
  createTimelineStoreCache,
  type TimelineStoreCache,
} from './stores/timeline-cache'

/**
 * The app's service graph — auth seam, API client, and stores — built once at
 * startup and provided via context. Tests build the same graph over msw and
 * an in-memory storage; components never construct services themselves.
 */
export interface AppServices {
  auth: CompositeAuthProvider
  api: ApiClient
  settings: SettingsStore
  accounts: AccountsStore
  rooms: RoomsStore
  search: SearchStore
  threadUnread: ThreadUnreadStore
  live: LiveConnection
  deviceState: DeviceStateStore
  ephemeral: EphemeralStore
  /** Outbound read receipts + typing notices to the homeserver (ADR 0067/0068). */
  ephemeralSender: EphemeralSender
  media: MediaService
  /**
   * Warm per-room timeline stores across room switches (ADR 0085 phase 1).
   * `RoomPage` acquires from here instead of building a store per mount.
   */
  timelines: TimelineStoreCache
  /**
   * The room the user is currently viewing (`accountId/roomId`), or `null`.
   * Set by `RoomPage`; `RoomList` reads it to mark the open row.
   */
  activeRoom: Signal<string | null>
  /** The thread panel currently open, so live replies there do not badge it. */
  activeThread: Signal<ActiveThread | null>
  /**
   * Bumped to pull focus back to the open room's composer (ADR 0078) — the
   * staged Escape's last stage, and where the room list's Escape lands. A
   * counter rather than a method so the composer can react to it as a signal;
   * the thread panel's composer deliberately ignores it.
   */
  composerFocus: Signal<number>
}

/**
 * `VITE_AXON_SERVER_URL` bakes a cross-origin server base into the bundle for
 * separately-hosted deployments (which rely on the server's CORS allow-list,
 * M-W1.5). Unset, the client is same-origin: the Vite dev proxy in
 * development, or a reverse proxy serving app and API together.
 *
 * This is the one accessor every server-base-URL consumer (HTTP client,
 * media, OAuth's `baseUrl`, WebSocket) should go through, so a future build
 * target with a non-origin base (e.g. Tauri, M-W12) only ever needs to set
 * this env var — no call site changes.
 */
export function apiBaseUrl(): string {
  return import.meta.env.VITE_AXON_SERVER_URL ?? '/'
}

function oauthClientId(): string {
  return import.meta.env.VITE_AXON_OAUTH_CLIENT_ID ?? 'axon-web'
}

/**
 * The OAuth redirect callback's origin. Unset, same-origin (today's default).
 * A future Tauri build (M-W12) would set this to a deep-link scheme instead
 * of implementing that here — see `OAuthAuthOptions.redirectUriBase`.
 */
function oauthRedirectUriBase(): string {
  return import.meta.env.VITE_AXON_OAUTH_REDIRECT_URI ?? window.location.origin
}

/** Apply server-derived room unread counts from the live bus (ADR 0070). */
export function connectUnreadCounts(
  live: LiveConnection,
  rooms: RoomsStore,
): () => void {
  return live.subscribe((frame) => {
    const counts = unreadCountsChange(frame)
    if (counts === null) {
      return
    }
    rooms.noteUnreadCounts(
      frame.accountId,
      counts.roomId,
      counts.notificationCount,
      counts.highlightCount,
    )
  })
}

/**
 * Feed live thread replies into the unread-thread store. The open thread is
 * skipped: `ThreadPanel` is the read surface for hidden replies, matching the
 * TUI's unread-thread semantics.
 */
export function connectLiveThreadUnread(
  live: LiveConnection,
  rooms: RoomsStore,
  accounts: AccountsStore,
  threadUnread: ThreadUnreadStore,
  activeThread: Signal<ActiveThread | null>,
): () => void {
  return live.subscribe((frame) => {
    const event = timelineEvent(frame)
    if (event === null) {
      return
    }
    const room = rooms.rooms.value.find(
      (candidate) =>
        candidate.account_id === event.account_id &&
        candidate.room_id === event.room_id,
    )
    // Two sources for "which user is me on this account": the accounts store
    // (keyed by the frame's account_id) with the room row as fallback. With
    // neither loaded yet the sender can't be attributed, and an unattributable
    // reply must not badge — it could be the user's own send from another
    // device; a genuinely-unread thread resurfaces on the next reply or via
    // summary reconciliation.
    const ownUserId =
      accounts.accounts.value.find((a) => a.account_id === event.account_id)
        ?.user_id ??
      room?.account_user_id ??
      null
    if (ownUserId === null) {
      return
    }
    threadUnread.recordLiveEvent(event, {
      roomTitle:
        room === undefined
          ? event.room_id
          : roomTitle(room, rooms.titles.value),
      ownUserId,
      activeThread: activeThread.value,
    })
  })
}

/**
 * Clear a room's server-backed unread badge when a *sibling* device reports
 * reading it (M-W6 step 5c, ADR 0048): a `read_markers` `device_state.changed`
 * frame from another device means the user has seen that room elsewhere.
 * Returns the unsubscribe; the app graph keeps the subscription for its life.
 */
export function connectReadMarkers(
  live: LiveConnection,
  deviceState: DeviceStateStore,
  rooms: RoomsStore,
): () => void {
  return live.subscribe((frame) => {
    const change = deviceStateChange(frame)
    if (
      change === null ||
      change.namespace !== READ_MARKERS_NAMESPACE ||
      change.deviceId === deviceState.deviceId
    ) {
      return
    }
    for (const [roomId, value] of Object.entries(change.entries)) {
      if (value !== null) {
        rooms.noteUnreadCounts(frame.accountId, roomId, 0, 0)
      }
    }
  })
}

/** Apply sibling devices' thread-read markers to local unread-thread state. */
export function connectThreadReadMarkers(
  live: LiveConnection,
  threadUnread: ThreadUnreadStore,
  deviceState: DeviceStateStore,
): () => void {
  return live.subscribe((frame) => {
    const change = deviceStateChange(frame)
    if (
      change === null ||
      change.namespace !== THREAD_READ_MARKERS_NAMESPACE ||
      change.deviceId === deviceState.deviceId
    ) {
      return
    }
    for (const value of Object.values(change.entries)) {
      const marker = parseThreadReadMarker(value)
      if (marker !== null) {
        threadUnread.markThreadRead(
          frame.accountId,
          marker.roomId,
          marker.rootEventId,
        )
      }
    }
  })
}

/**
 * Keep the room list live (M-W6 follow-up, WCR-08): every `timeline.event`
 * frame advances its room's `last_activity_ts` and latest-message preview (so
 * the default "recent" sort and optional preview stay recent through a
 * session; an unknown room triggers a list re-read — how a newly joined room
 * appears), and a reconnect re-reads the list outright — the bus is lossy, and
 * joins/renames missed while disconnected have no other repair path. Returns
 * the unsubscribe; the app graph keeps the subscription for its life.
 */
export function connectLiveRooms(
  live: LiveConnection,
  rooms: RoomsStore,
): () => void {
  const unsubscribe = live.subscribe((frame) => {
    const event = timelineEvent(frame)
    if (event === null) {
      return
    }
    rooms.noteTimelineEvent(event)
  })
  const dispose = effect(() => {
    if (live.reconnects.value === 0) {
      return
    }
    void rooms.refresh()
  })
  return () => {
    unsubscribe()
    dispose()
  }
}

/**
 * Drop every warm timeline store when the session ends (ADR 0085 phase 1).
 *
 * The service graph outlives a sign-out — nothing reloads the document, the
 * signed-in branch of `app.tsx` just unmounts — so before phase 1 a signed-out
 * tab kept no messages at all: each `RoomPage` store died with its mount.
 * Warming the stores changes that, and this restores it. It is the in-memory
 * half of the rule ADR 0085 states for the persisted cache: wipe on *any*
 * logout or token change, all accounts, not only the one that signed out.
 *
 * Returns the disposer; the app graph keeps the subscription for its life.
 */
export function connectTimelineCacheReset(
  auth: CompositeAuthProvider,
  timelines: TimelineStoreCache,
): () => void {
  return effect(() => {
    if (auth.signedIn.value) {
      return
    }
    // Runs inside the reactive flush, which is safe only because `clear()` is
    // *idempotent*: disposing a store that holds a local echo does write a
    // signal, but the map is emptied, so a re-run of this effect within the
    // same flush writes nothing and the flush settles. An unconditional write
    // here would not settle — @preact/signals gives up after 100 flush
    // iterations with "Cycle detected" — which is exactly what an unguarded
    // wipe did in `connectAttachmentReset`. Keep any wipe reached from here
    // a no-op once it has nothing left to do.
    timelines.clear()
  })
}

/** Route raw Matrix ephemeral passthrough frames into the web overlay store. */
export function connectEphemeralPassthrough(
  live: LiveConnection,
  ephemeral: EphemeralStore,
): () => void {
  const unsubscribe = live.subscribe((frame) => {
    const event = ephemeralPassthrough(frame)
    if (event !== null) {
      ephemeral.apply(frame.accountId, event)
    }
  })
  const dispose = effect(() => {
    if (live.reconnects.value > 0 || live.connection.value !== 'live') {
      ephemeral.clearTyping()
    }
  })
  return () => {
    unsubscribe()
    dispose()
  }
}

export function createServices(
  storage: Storage = window.localStorage,
  sessionStorage: Storage = window.sessionStorage,
): AppServices {
  const auth = createCompositeAuthProvider({
    providers: parseOAuthProviders(import.meta.env.VITE_AXON_OAUTH_PROVIDERS),
    baseUrl: apiBaseUrl(),
    clientId: oauthClientId(),
    redirectUriBase: oauthRedirectUriBase(),
    storage,
    sessionStorage,
  })
  const api = createApiClient(auth, apiBaseUrl())
  const media = createMediaService({ auth, baseUrl: apiBaseUrl() })
  const timelines = createTimelineStoreCache(api, media)
  const settings = createSettingsStore(storage)
  const accounts = createAccountsStore(api)
  const rooms = createRoomsStore(api, storage)
  const search = createSearchStore(api)
  const threadUnread = createThreadUnreadStore()
  const ephemeral = createEphemeralStore()
  const ephemeralSender = createEphemeralSender(api)
  // Same-origin by default (the dev/reverse proxy); a cross-origin base is set
  // for separately-hosted deployments, exactly as the HTTP client resolves it.
  const live = createLiveConnection({
    socketFactory: () => {
      // A WebSocket bakes the token into its subprotocols at construction, so
      // the token must be available synchronously. Token-paste (M-W6) is sync;
      // an async provider (the Tauri keychain, M-W12) needs an async open path
      // that does not exist yet — until then it degrades to `offline` here.
      const token = auth.getToken()
      if (typeof token !== 'string') {
        throw new Error('live socket requires a synchronously available token')
      }
      return openLiveSocket(token, apiBaseUrl())
    },
  })
  const activeRoom = signal<string | null>(null)
  const activeThread = signal<ActiveThread | null>(null)
  const composerFocus = signal(0)
  const deviceState = createDeviceStateStore(api, live, storage)
  connectUnreadCounts(live, rooms)
  connectLiveRooms(live, rooms)
  connectLiveThreadUnread(live, rooms, accounts, threadUnread, activeThread)
  connectEphemeralPassthrough(live, ephemeral)
  connectReadMarkers(live, deviceState, rooms)
  connectThreadReadMarkers(live, threadUnread, deviceState)
  connectTimelineCacheReset(auth, timelines)
  return {
    auth,
    api,
    media,
    timelines,
    settings,
    accounts,
    rooms,
    search,
    threadUnread,
    live,
    deviceState,
    ephemeral,
    ephemeralSender,
    activeRoom,
    activeThread,
    composerFocus,
  }
}

export const ServicesContext = createContext<AppServices | null>(null)

export function useServices(): AppServices {
  const services = useContext(ServicesContext)
  if (services === null) {
    throw new Error('useServices called outside <ServicesContext.Provider>')
  }
  return services
}
