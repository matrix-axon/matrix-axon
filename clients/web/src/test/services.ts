import { signal } from '@preact/signals'
import { createApiClient } from '../api/client'
import { createCompositeAuthProvider } from '../auth/composite'
import type { OAuthProviderConfig } from '../auth/oauth'
import type { CacheStore } from '../stores/cache-store'
import {
  connectCacheReset,
  connectCacheSetting,
  connectRoomsSessionReset,
  connectMatrixOAuthQrSessionReset,
  connectInvitesSessionReset,
  connectLiveInvites,
  connectLiveRooms,
  connectLiveThreadUnread,
  connectEphemeralPassthrough,
  connectReadMarkers,
  connectAttachmentReset,
  connectThreadReadMarkers,
  connectTimelineCacheReset,
  connectUnreadCounts,
  connectUpdateChecks,
  type AppServices,
} from '../services'
import { setPerfEnabled } from '../perf'
import { createAccountsStore } from '../stores/accounts'
import { createMatrixOAuthQrStore } from '../stores/matrix-oauth-qr'
import { createDeviceStateStore } from '../stores/device-state'
import { createEphemeralStore } from '../stores/ephemeral'
import { createEphemeralSender } from '../stores/ephemeral-sender'
import { createMediaService } from '../media/media-service'
import { createLiveConnection } from '../stores/live-connection'
import { cacheNamespace, createMemoryCacheStore } from '../stores/cache-store'
import { createTelemetryStore } from '../stores/telemetry'
import { createRoomListCache } from '../stores/room-list-cache'
import { createInvitesStore } from '../stores/invites'
import { createRoomsStore } from '../stores/rooms'
import { createSearchStore } from '../stores/search'
import { createSettingsStore } from '../stores/settings'
import { createSpacesStore } from '../stores/spaces'
import {
  createThreadUnreadStore,
  type ActiveThread,
} from '../stores/thread-unread'
import { createTimelineStoreCache } from '../stores/timeline-cache'
import {
  createUpdateChecker,
  type VersionManifest,
} from '../stores/update-check'
import { createAttachmentStaging } from '../media/attachment-staging'
import { createBrowserQrAdapter, type BrowserQrAdapter } from '../qr/browser-qr'
import { FakeWebSocket } from './fake-socket'
import { memoryStorage } from './memory-storage'

/** msw handlers in component tests register against this origin. */
export const TEST_BASE_URL = 'http://axon.test'

/** Deactivated-shaped `AccountDto.backup` (ADR 0098). Typed fixtures need it. */
export const UNKNOWN_BACKUP = {
  exists_on_server: null,
  this_device_uploading: false,
  backup_state: 'unknown',
  recovery_state: 'unknown',
} as const

/**
 * The real service graph over an in-memory storage and the msw base URL —
 * what `createServices` builds in production, minus the browser globals.
 */
export function testServices(
  options: {
    token?: string | null
    oauthProviders?: readonly OAuthProviderConfig[]
    pendingStorage?: Storage
    storage?: Storage
    /**
     * A pre-seeded durable cache (ADR 0085 phase 2). Defaults to an empty
     * in-memory adapter, so every graph exercises the real cache path with no
     * `fake-indexeddb` and no jsdom IDB quirks.
     */
    cache?: CacheStore
    /**
     * What the origin reports as its build. Defaults to learning nothing, so
     * no test sees an update — or a reload — it did not ask for.
     */
    versionManifest?: () => Promise<VersionManifest | null>
    /** The build the bundle claims to be, for update comparisons. */
    currentVersion?: string
    /** Injectable browser boundary for QR page interaction tests. */
    qr?: BrowserQrAdapter
  } = {},
): AppServices & { sockets: FakeWebSocket[] } {
  const storage =
    options.storage ??
    memoryStorage(
      options.token === null
        ? {}
        : { 'axon.token': options.token ?? 'tok-test' },
    )
  const auth = createCompositeAuthProvider({
    providers: options.oauthProviders ?? [],
    baseUrl: TEST_BASE_URL,
    storage,
    // Isolate the session tier too: without this, session-mode token writes
    // (rememberMe=false) land in jsdom's real shared `window.sessionStorage`
    // and leak signed-in state across tests.
    sessionStorage: memoryStorage(),
    pendingStorage: options.pendingStorage ?? memoryStorage(),
  })
  const api = createApiClient(auth, TEST_BASE_URL)
  const media = createMediaService({ auth, baseUrl: TEST_BASE_URL })
  const timelines = createTimelineStoreCache(api, media)
  const attachments = createAttachmentStaging()
  const settings = createSettingsStore(storage)
  // Mirrors `createServices`: instrumentation on before the stores that mark.
  if (settings.perfMarks.peek()) {
    setPerfEnabled(true)
  }
  const accounts = createAccountsStore(api)
  const matrixOAuthQr = createMatrixOAuthQrStore(api, accounts, {
    storage: options.pendingStorage ?? memoryStorage(),
  })
  const qr = options.qr ?? createBrowserQrAdapter()
  const cache = options.cache ?? createMemoryCacheStore()
  const telemetry = createTelemetryStore({
    cache,
    enabled: () => false,
  })
  const rooms = createRoomsStore(
    api,
    storage,
    createRoomListCache({
      cache,
      namespace: () =>
        Promise.resolve(auth.getToken()).then(
          (token) => cacheNamespace(TEST_BASE_URL, token),
          () => null,
        ),
      enabled: () => settings.cacheRoomList.peek(),
    }),
  )
  const search = createSearchStore(api)
  const threadUnread = createThreadUnreadStore()
  const ephemeral = createEphemeralStore()
  const ephemeralSender = createEphemeralSender(api)
  // An inert socket: `start()` builds one but nothing drives its handshake, so
  // no frames flow unless a test reaches for the fake and emits them. jsdom has
  // no real `WebSocket`, so this keeps the graph constructible everywhere.
  // Created sockets are collected on `sockets` so a test can drive frames:
  // `services.live.start(); services.sockets[0].emitOpen(); …emitMessage(…)`.
  const sockets: FakeWebSocket[] = []
  const live = createLiveConnection({
    socketFactory: () => {
      const socket = new FakeWebSocket()
      sockets.push(socket)
      return socket.asWebSocket()
    },
  })
  const activeRoom = signal<string | null>(null)
  const activeThread = signal<ActiveThread | null>(null)
  const composerFocus = signal(0)
  const deviceState = createDeviceStateStore(api, live, storage)
  const spaces = createSpacesStore(api, rooms, live)
  const updates = createUpdateChecker({
    currentVersion: options.currentVersion ?? 'test-build',
    fetchManifest: options.versionManifest ?? (() => Promise.resolve(null)),
  })
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
  connectMatrixOAuthQrSessionReset(auth, matrixOAuthQr)
  connectCacheSetting(settings, cache)
  connectUpdateChecks(live, updates)
  connectAttachmentReset(auth, attachments)
  return {
    auth,
    api,
    media,
    updates,
    timelines,
    cache,
    telemetry,
    attachments,
    settings,
    accounts,
    matrixOAuthQr,
    qr,
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
    sockets,
  }
}
