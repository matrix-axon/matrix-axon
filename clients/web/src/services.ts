import { effect, signal, type Signal } from '@preact/signals'
import { createContext } from 'preact'
import { useContext } from 'preact/hooks'
import { createApiClient, type ApiClient } from './api/client'
import {
  deviceStateChange,
  ephemeralPassthrough,
  inviteAdded,
  inviteRemoved,
  timelineEvent,
  unreadCountsChange,
} from './api/frames'
import { openLiveSocket } from './api/ws'
import { setPerfEnabled } from './perf'
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
import {
  cacheNamespace,
  createIndexedDbCacheStore,
  requestPersistentStorage,
  type CacheStore,
} from './stores/cache-store'
import { roomTitle } from './stores/room-list'
import { createRoomListCache } from './stores/room-list-cache'
import { createInvitesStore, type InvitesStore } from './stores/invites'
import { createRoomsStore, type RoomsStore } from './stores/rooms'
import { createSearchStore, type SearchStore } from './stores/search'
import { createSettingsStore, type SettingsStore } from './stores/settings'
import { createSpacesStore, type SpacesStore } from './stores/spaces'
import {
  createThreadUnreadStore,
  type ActiveThread,
  type ThreadUnreadStore,
} from './stores/thread-unread'
import {
  createTimelineStoreCache,
  type TimelineStoreCache,
} from './stores/timeline-cache'
import {
  createUpdateChecker,
  fetchVersionManifest,
  VERSION_MANIFEST_PATH,
  type UpdateChecker,
} from './stores/update-check'
import { BUILD_INFO } from './build-info'

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
  invites: InvitesStore
  spaces: SpacesStore
  search: SearchStore
  threadUnread: ThreadUnreadStore
  live: LiveConnection
  deviceState: DeviceStateStore
  ephemeral: EphemeralStore
  /** Outbound read receipts + typing notices to the homeserver (ADR 0067/0068). */
  ephemeralSender: EphemeralSender
  media: MediaService
  /**
   * Watches the origin for a newer web build (ADR 0087). Detection only — the
   * reload policy lives in `startAutoRefresh` (`src/update-refresh.ts`).
   */
  updates: UpdateChecker
  /**
   * Warm per-room timeline stores across room switches (ADR 0085 phase 1).
   * `RoomPage` acquires from here instead of building a store per mount.
   */
  timelines: TimelineStoreCache
  /**
   * The durable on-device content cache (ADR 0085 phase 2). Stores reach it
   * through narrow ports (`RoomListCache`); it is exposed here for the
   * lifecycle wiring — the sign-out wipe and the setting — and for phase 3.
   */
  cache: CacheStore
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
 * Re-check the origin's build whenever the live socket comes back (ADR 0087).
 *
 * This is the fast path, and it is nearly free. The web bundle and the `/v1`
 * API are served through one front door, so pushing a new build restarts the
 * process the socket runs through and every client's socket drops. A reconnect
 * is therefore the earliest evidence a deploy happened — seconds, with no
 * polling — and on the far more common case of an ordinary network blip the
 * check costs one conditional request that answers "same build".
 *
 * Returns the unsubscribe; the app graph keeps it for its life.
 */
export function connectUpdateChecks(
  live: LiveConnection,
  updates: UpdateChecker,
): () => void {
  return effect(() => {
    if (live.reconnects.value === 0) {
      return
    }
    void updates.check()
  })
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

/**
 * Wipe the **whole** durable cache when the session ends (ADR 0085 phase 2).
 *
 * Not just the records belonging to the account that signed out: the keys are
 * per-reader and a surgical drop would work, but whole-cache is the
 * conservative choice and the cache rebuilds itself from the next refresh at
 * the cost of one cold start. A wipe that is too broad is invisible to the
 * user; a wipe that misses something is a privacy failure.
 *
 * Unlike `connectTimelineCacheReset` this writes no signals — `clear()` is an
 * IndexedDB round trip — so it cannot re-enter the reactive flush, and needs
 * no idempotence guard for that reason. It still runs for a visitor who was
 * never signed in, which is deliberate: a cache outliving an evicted session
 * is exactly what should be dropped on sight.
 */
export function connectCacheReset(
  auth: CompositeAuthProvider,
  cache: CacheStore,
): () => void {
  return effect(() => {
    if (!auth.signedIn.value) {
      void cache.clear()
    }
  })
}

/**
 * Abandon the room list when the session ends (ADR 0085 phase 2).
 *
 * The service graph outlives a sign-out, so the rooms store would otherwise
 * carry one reader's rows into the next reader's session — and a `/v1/rooms`
 * request still in flight under the old token would land *after* the wipe and
 * be persisted under the new reader's cache key, which no amount of wiping at
 * sign-out can catch. `resetSession` bumps a generation those completions are
 * checked against, so they are discarded instead.
 *
 * Safe inside the reactive flush for the same reason `connectTimelineCacheReset`
 * is: `resetSession` is idempotent, so a re-run within one flush writes no
 * signals and the flush settles.
 *
 * This assumes a token change always passes through signed-out — true today,
 * since the paste form only renders when there is no usable token.
 */
export function connectRoomsSessionReset(
  auth: CompositeAuthProvider,
  rooms: RoomsStore,
): () => void {
  return effect(() => {
    if (!auth.signedIn.value) {
      rooms.resetSession()
    }
  })
}

export function connectInvitesSessionReset(
  auth: CompositeAuthProvider,
  invites: InvitesStore,
): () => void {
  return effect(() => {
    if (!auth.signedIn.value) {
      invites.resetSession()
    }
  })
}

/** Keep the invite inbox live (ADR 0091). Reconnect re-reads the list. */
export function connectLiveInvites(
  live: LiveConnection,
  invites: InvitesStore,
): () => void {
  const unsubscribe = live.subscribe((frame) => {
    const added = inviteAdded(frame)
    if (added !== null) {
      invites.noteAdded(added)
      return
    }
    const removed = inviteRemoved(frame)
    if (removed !== null) {
      invites.noteRemoved(frame.accountId, removed.roomId)
    }
  })
  const dispose = effect(() => {
    if (live.reconnects.value === 0) {
      return
    }
    void invites.refresh()
  })
  return () => {
    unsubscribe()
    dispose()
  }
}

/**
 * Honor the room-list cache setting: turning it off must remove what is
 * already on disk, not merely stop adding to it. Turning it back on rebuilds
 * from the next refresh.
 */
export function connectCacheSetting(
  settings: SettingsStore,
  cache: CacheStore,
): () => void {
  return effect(() => {
    if (!settings.cacheRoomList.value) {
      void cache.clear()
    }
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
  // Deliberately *not* `apiBaseUrl()`: the manifest describes the web bundle,
  // which is served by the document's own origin even in a cross-origin
  // deployment where the API lives elsewhere (`VITE_AXON_SERVER_URL`).
  const updates = createUpdateChecker({
    currentVersion: BUILD_INFO.version,
    fetchManifest: () => fetchVersionManifest(VERSION_MANIFEST_PATH),
    isVisible: () => !document.hidden,
  })
  const timelines = createTimelineStoreCache(api, media)
  const settings = createSettingsStore(storage)
  // Instrumentation has to be live *before* anything worth measuring happens.
  // The room-list cache marks its read during this function, and `App`'s
  // effect — the other place the stored preference is applied — runs after
  // mount, far too late: those marks were silently dropped, and the ADR 0085
  // boot summary reported `read=null` on every device that enabled perf from
  // Settings rather than `?perf=1` (which latches inside `perfEnabled`).
  //
  // Only ever turns it *on*: `?perf=1` may already have latched, and passing
  // `false` here would clobber it.
  if (settings.perfMarks.peek()) {
    setPerfEnabled(true)
  }
  const accounts = createAccountsStore(api)
  const cache = createIndexedDbCacheStore()
  requestPersistentStorage()
  // Resolved per operation rather than once: token-paste sign-in does not
  // reload the document, so a graph built while signed out would hold `null`
  // for the whole session and never write a thing — leaving the *next* launch
  // cold too, which on a phone is a launch the user sees.
  const namespace = () =>
    Promise.resolve(auth.getToken()).then(
      (token) => cacheNamespace(apiBaseUrl(), token),
      () => null,
    )
  const rooms = createRoomsStore(
    api,
    storage,
    createRoomListCache({
      cache,
      namespace,
      enabled: () => settings.cacheRoomList.peek(),
    }),
  )
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
  const spaces = createSpacesStore(api, rooms, live)
  const invites = createInvitesStore(api, rooms)
  connectUnreadCounts(live, rooms)
  connectLiveRooms(live, rooms)
  connectLiveInvites(live, invites)
  connectInvitesSessionReset(auth, invites)
  connectLiveThreadUnread(live, rooms, accounts, threadUnread, activeThread)
  connectEphemeralPassthrough(live, ephemeral)
  connectReadMarkers(live, deviceState, rooms)
  connectThreadReadMarkers(live, threadUnread, deviceState)
  connectTimelineCacheReset(auth, timelines)
  connectCacheReset(auth, cache)
  connectRoomsSessionReset(auth, rooms)
  connectCacheSetting(settings, cache)
  connectUpdateChecks(live, updates)
  return {
    auth,
    api,
    media,
    updates,
    timelines,
    cache,
    settings,
    accounts,
    rooms,
    invites,
    spaces,
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
