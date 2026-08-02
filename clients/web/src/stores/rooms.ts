import {
  computed,
  signal,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import { perfMark, perfMarkBootRoomList } from '../perf'
import type { components } from '../api/schema'
import { apiErrorCode, apiErrorMessage, type ApiClient } from '../api/client'
import { markdownToPlainText } from '../markdown/markdown'
import {
  dmTitleFromMembers,
  isLikelyDm,
  memberDisplay,
  roomKey,
  roomTitle,
  type MemberDto,
  type RoomDto,
} from './room-list'
import type { RoomListCache } from './room-list-cache'
import type { EventDto } from './timeline'

export interface RoomPreview {
  eventId: string
  senderDisplay: string
  body: string
}

export interface RoomUnreadCounts {
  notificationCount: number
  highlightCount: number
}

export type RoomMembershipResult = { ok: true } | { ok: false; message: string }
export type RoomEntryResult =
  | { ok: true; roomId: string }
  | { ok: false; message: string; code: string | null }
export type CreateRoomRequest = components['schemas']['CreateRoomRequestDto']

export interface RoomsStore {
  rooms: ReadonlySignal<RoomDto[]>
  /** Rooms whose server-derived notification count is nonzero. */
  unreadKeys: ReadonlySignal<ReadonlySet<string>>
  /**
   * Sum of every room's notification count (ADR 0080) — a message-volume
   * total, unlike `unreadKeys.size`'s room-volume total. Used for the app
   * icon badge, where a stuck "1" while messages pile up in one room reads as
   * broken.
   */
  unreadTotal: ReadonlySignal<number>
  /**
   * True while there is *nothing to show* — not "a request is in flight".
   * Restoring the cache clears it before the network answers (ADR 0085
   * phase 2), which is the whole point: cached rows are something to show.
   */
  loading: ReadonlySignal<boolean>
  /**
   * True while the rows on screen came from the cache and no refresh has
   * confirmed them yet. Set by a restore, cleared by the first successful
   * refresh, and deliberately *not* cleared by a failed one — cached content
   * survives a failed refresh, still marked stale.
   */
  stale: ReadonlySignal<boolean>
  error: Signal<string | null>
  /**
   * Member-derived titles for unnamed rooms, keyed by room key. Read through
   * `roomTitle(room, titles.value)`.
   */
  titles: ReadonlySignal<ReadonlyMap<string, string>>

  /** Fetch the room list (server-side membership filtering per ADR 0037). */
  refresh(): Promise<void>
  /**
   * Abandon everything belonging to the session that just ended.
   *
   * The service graph outlives a sign-out — nothing reloads the document — so
   * without this the next reader inherits the previous one's rows, and, worse,
   * a `/v1/rooms` request still in flight under the *old* token would land in
   * the new session and be persisted under the new reader's cache key. Bumps a
   * generation that in-flight completions are checked against, so those
   * responses are discarded rather than applied.
   *
   * Idempotent: calling it on an already-clean store writes no signals, which
   * is what lets it be driven from an effect (ADR 0085 phase 1's lesson —
   * an unconditional write there does not settle).
   */
  resetSession(): void
  /**
   * Refresh unless one has already settled this session, and resolve when the
   * list is as current as this store can make it.
   *
   * Every "load it if nobody else has" call site must go through this rather
   * than testing `loading` or `rooms.length`: a cache restore (ADR 0085 phase
   * 2) makes both of those say "loaded" while the rows are still unconfirmed,
   * so a guard written against them would leave the cached list to stand
   * forever on any route that is the only thing asking (issue #101) — and a
   * bulk mutation computed from it would write to a stale copy and report
   * success (issue #102).
   */
  ensureLoaded(): Promise<void>
  /** Leave one room through M19b, then refresh authoritative room state. */
  leaveRoom(accountId: string, roomId: string): Promise<RoomMembershipResult>
  /** Forget one left/banned room through M19b, then refresh room state. */
  forgetRoom(accountId: string, roomId: string): Promise<RoomMembershipResult>
  /** Join a room by id or alias through M19c, then refresh room state. */
  joinRoom(
    accountId: string,
    roomIdOrAlias: string,
    serverNames?: readonly string[],
  ): Promise<RoomEntryResult>
  /** Knock on a room by id or alias through M19c, then refresh room state. */
  knockRoom(
    accountId: string,
    roomIdOrAlias: string,
    reason?: string | null,
    serverNames?: readonly string[],
  ): Promise<RoomEntryResult>
  /** Create a room through M19c, then refresh room state. */
  createRoom(
    accountId: string,
    request: CreateRoomRequest,
  ): Promise<RoomEntryResult>
  /** Create a DM through M19c, then refresh room state. */
  createDm(accountId: string, userId: string): Promise<RoomEntryResult>
  /** Latest-message preview for one room. */
  preview(key: string): RoomPreview | undefined
  /** Server-derived unread notification count for one room. */
  unreadCount(key: string): number
  /** Fetch the latest-message preview for one room, if it is not cached. */
  hydratePreview(room: RoomDto): void
  /**
   * A live `timeline.event` for `(accountId, roomId)` at `ts` (M-W6 follow-up,
   * WCR-08): advance the room's `last_activity_ts` so the default "recent"
   * sort stays recent through a session, or — for a room not in the list —
   * trigger a refresh, which is how a newly joined room appears without a
   * reload.
   */
  noteActivity(accountId: string, roomId: string, ts: number): void
  /** Apply a live timeline event to room activity and preview state. */
  noteTimelineEvent(event: EventDto): void
  /** Apply a live or optimistic unread-count update for one room. */
  noteUnreadCounts(
    accountId: string,
    roomId: string,
    notificationCount: number,
    highlightCount: number,
  ): void
}

/** How many member-list fetches run at once when resolving DM titles. */
const TITLE_FETCH_CONCURRENCY = 4
const PREVIEW_PAGE_LIMIT = 50
const PREVIEW_MAX_PAGES = 3
const ROOM_TITLE_CACHE_KEY = 'axon.room_titles.v1'

/**
 * The cross-account room list (ADR 0046, M-W4). After each refresh, rooms
 * with no name/alias (likely DMs, ADR 0042 heuristic) get their titles
 * resolved from their member lists in the background — the TUI's
 * `request_unnamed_room_titles` behavior. Once cached, a title is not
 * re-fetched; the cache lives for the store's lifetime.
 */
export function createRoomsStore(
  api: ApiClient,
  storage: Storage = window.localStorage,
  cache: RoomListCache | null = null,
): RoomsStore {
  const rooms = signal<RoomDto[]>([])
  const unreadKeys = signal<ReadonlySet<string>>(new Set())
  /** Sum of every room's `notificationCount` (ADR 0080's app-icon badge). */
  const unreadTotal = signal(0)
  const loading = signal(true)
  const stale = signal(false)
  const error = signal<string | null>(null)
  const titles = signal<ReadonlyMap<string, string>>(loadTitleCache(storage))
  /** Keys with a fetch in flight or already settled (including "no title"). */
  const requested = new Set<string>()
  /** Preview cache keys: `${roomKey}\0${last_event_id}`. */
  const requestedPreviews = new Set<string>()
  /** One preview signal per room, so a live event wakes only its row. */
  const previewSlots = new Map<string, Signal<RoomPreview | undefined>>()
  /** One unread-count signal per room, so a count update wakes only its row. */
  const unreadSlots = new Map<string, Signal<RoomUnreadCounts>>()
  /** Member lists by `roomKey(room)`, shared by every preview for that room. */
  const members = new Map<string, Promise<MemberDto[]>>()
  /** Rooms this session successfully left/forgot before sync caught up. */
  const locallyHiddenRooms = new Set<string>()
  /**
   * Room creation can return before sync has reflected the m.room.name/topic
   * events into `/v1/rooms`. Keep the user's explicit create-room metadata as
   * a local fill-only overlay so a named one-invite room doesn't temporarily
   * render as a member-derived DM.
   */
  const locallyCreatedRoomMetadata = new Map<
    string,
    { name: string | null; topic: string | null }
  >()

  async function fetchTitle(room: RoomDto): Promise<void> {
    let data
    try {
      ;({ data } = await api.GET(
        '/v1/accounts/{account_id}/rooms/{room_id}/members',
        {
          params: {
            path: { account_id: room.account_id, room_id: room.room_id },
          },
        },
      ))
    } catch {
      // Network-level failure: a rejection here would otherwise fall out of
      // `resolveUnnamedTitles`'s fire-and-forget `Promise.all` unhandled
      // (WCR-02). Same policy as the envelope-error path below.
      return
    }
    if (data === undefined) {
      // Error envelope. Leave `requested` set: a failing room is not retried
      // this session (matching the TUI's per-room rate limiting in spirit);
      // the room id remains the fallback title.
      return
    }
    const title = dmTitleFromMembers(room.account_user_id, data.data)
    if (title !== null) {
      setCachedTitle(roomKey(room), title)
    }
  }

  function setCachedTitle(key: string, title: string): void {
    const trimmed = title.trim()
    if (trimmed === '') {
      return
    }
    const next = new Map(titles.value).set(key, trimmed)
    titles.value = next
    saveTitleCache(storage, next)
  }

  function cacheRoomListTitles(current: RoomDto[]): void {
    const next = new Map(titles.value)
    let changed = false
    for (const room of current) {
      if (!isLikelyDm(room)) {
        const title = roomTitle(room, next)
        if (title !== room.room_id && next.get(roomKey(room)) !== title) {
          next.set(roomKey(room), title)
          changed = true
        }
      }
    }
    if (changed) {
      titles.value = next
      saveTitleCache(storage, next)
    }
  }

  function applyLocalRoomMetadata(current: RoomDto[]): RoomDto[] {
    let changed = false
    const next = current.map((room) => {
      const key = roomKey(room)
      const metadata = locallyCreatedRoomMetadata.get(key)
      if (metadata === undefined) {
        return room
      }
      const name = blankRoomField(room.name) ? metadata.name : room.name
      const topic = blankRoomField(room.topic) ? metadata.topic : room.topic
      if (name === room.name && topic === room.topic) {
        return room
      }
      changed = true
      return { ...room, name, topic }
    })
    return changed ? next : current
  }

  async function resolveUnnamedTitles(current: RoomDto[]): Promise<void> {
    const queue = current.filter(
      (room) => isLikelyDm(room) && !requested.has(roomKey(room)),
    )
    for (const room of queue) {
      requested.add(roomKey(room))
    }
    // Bounded parallelism: N workers draining one shared queue.
    let next = 0
    const worker = async () => {
      while (next < queue.length) {
        const room = queue[next]
        next += 1
        await fetchTitle(room)
      }
    }
    await Promise.all(
      Array.from({ length: TITLE_FETCH_CONCURRENCY }, () => worker()),
    )
  }

  /**
   * The room's member list, fetched once and cached for the store's lifetime.
   * Previews re-resolve on every new message; the names behind them do not, so
   * this must not become a `/members` round trip per inbound event.
   */
  function roomMembers(room: RoomDto): Promise<MemberDto[]> {
    const key = roomKey(room)
    const cached = members.get(key)
    if (cached !== undefined) {
      return cached
    }
    const pending = api
      .GET('/v1/accounts/{account_id}/rooms/{room_id}/members', {
        params: {
          path: { account_id: room.account_id, room_id: room.room_id },
        },
      })
      .then((result) => result.data?.data ?? [])
      .catch(() => {
        // Don't cache a failure: the next preview may as well try again.
        members.delete(key)
        return []
      })
    members.set(key, pending)
    return pending
  }

  function previewSlot(key: string): Signal<RoomPreview | undefined> {
    const existing = previewSlots.get(key)
    if (existing !== undefined) {
      return existing
    }
    const created = signal<RoomPreview | undefined>(undefined)
    previewSlots.set(key, created)
    return created
  }

  function unreadSlot(key: string): Signal<RoomUnreadCounts> {
    const existing = unreadSlots.get(key)
    if (existing !== undefined) {
      return existing
    }
    const created = signal<RoomUnreadCounts>({
      notificationCount: 0,
      highlightCount: 0,
    })
    unreadSlots.set(key, created)
    return created
  }

  function setUnreadCounts(
    key: string,
    notificationCount: number,
    highlightCount: number,
    nextUnreadKeys?: Set<string>,
  ): void {
    const slot = unreadSlot(key)
    const current = slot.value
    if (
      current.notificationCount !== notificationCount ||
      current.highlightCount !== highlightCount
    ) {
      unreadTotal.value += notificationCount - current.notificationCount
      slot.value = { notificationCount, highlightCount }
    }
    if (nextUnreadKeys !== undefined) {
      if (notificationCount > 0) {
        nextUnreadKeys.add(key)
      }
      return
    }
    const isUnread = unreadKeys.value.has(key)
    if (notificationCount > 0 === isUnread) {
      return
    }
    const next = new Set(unreadKeys.value)
    if (notificationCount > 0) {
      next.add(key)
    } else {
      next.delete(key)
    }
    unreadKeys.value = next
  }

  function syncUnreadCounts(current: RoomDto[]): void {
    const nextUnreadKeys = new Set<string>()
    const liveKeys = new Set<string>()
    for (const room of current) {
      const key = roomKey(room)
      liveKeys.add(key)
      setUnreadCounts(
        key,
        countFromRoom(room.notification_count),
        countFromRoom(room.highlight_count),
        nextUnreadKeys,
      )
    }
    for (const key of unreadSlots.keys()) {
      if (!liveKeys.has(key)) {
        setUnreadCounts(key, 0, 0, nextUnreadKeys)
      }
    }
    unreadKeys.value = nextUnreadKeys
  }

  function setPreview(key: string, preview: RoomPreview): void {
    const slot = previewSlot(key)
    const current = slot.value
    if (
      current !== undefined &&
      current.eventId === preview.eventId &&
      current.senderDisplay === preview.senderDisplay &&
      current.body === preview.body
    ) {
      return
    }
    slot.value = preview
  }

  async function fetchPreview(room: RoomDto, eventId: string): Promise<void> {
    let event: EventDto | undefined
    let roster: MemberDto[]
    try {
      ;[event, roster] = await Promise.all([
        fetchLatestPreviewEvent(room),
        roomMembers(room),
      ])
    } catch {
      return
    }
    const body = previewBody(event?.body)
    if (event === undefined || body === undefined || body === '') {
      return
    }
    const sender = event.sender
    const member = roster.find((candidate) => candidate.user_id === sender)
    const preview: RoomPreview = {
      eventId,
      senderDisplay: member === undefined ? sender : memberDisplay(member),
      body,
    }
    setPreview(roomKey(room), preview)
  }

  function setLivePreview(room: RoomDto, event: EventDto): void {
    const body = previewBody(event.body)
    if (!isPreviewEvent(event) || body === undefined || body === '') {
      return
    }
    const key = roomKey(room)
    setPreview(key, {
      eventId: event.event_id,
      senderDisplay: event.sender,
      body,
    })
    void roomMembers(room).then((roster) => {
      const member = roster.find(
        (candidate) => candidate.user_id === event.sender,
      )
      if (member === undefined) {
        return
      }
      setPreview(key, {
        eventId: event.event_id,
        senderDisplay: memberDisplay(member),
        body,
      })
    })
  }

  async function fetchLatestPreviewEvent(
    room: RoomDto,
  ): Promise<EventDto | undefined> {
    let cursor: string | undefined
    for (let page = 0; page < PREVIEW_MAX_PAGES; page += 1) {
      const { data } = await api.GET(
        '/v1/accounts/{account_id}/rooms/{room_id}/timeline',
        {
          params: {
            path: { account_id: room.account_id, room_id: room.room_id },
            query: { limit: PREVIEW_PAGE_LIMIT, cursor },
          },
        },
      )
      if (data === undefined) {
        return undefined
      }
      const event = data.data.events.find(isPreviewEvent)
      if (event !== undefined) {
        return event
      }
      cursor = data.data.next_cursor ?? undefined
      if (cursor === undefined) {
        return undefined
      }
    }
    return undefined
  }

  function hydratePreview(room: RoomDto): void {
    const eventId = room.last_event_id
    if (eventId === null || eventId === undefined || eventId === '') {
      return
    }
    const key = `${roomKey(room)}\0${eventId}`
    if (requestedPreviews.has(key)) {
      return
    }
    requestedPreviews.add(key)
    void fetchPreview(room, eventId)
  }

  /** The freshest row, so `noteActivity` can skip the common burst case. */
  let newestKey: string | null = null
  let newestTs = -Infinity

  function recomputeNewest(current: RoomDto[]): void {
    newestKey = null
    newestTs = -Infinity
    for (const room of current) {
      if (room.last_activity_ts > newestTs) {
        newestTs = room.last_activity_ts
        newestKey = roomKey(room)
      }
    }
  }

  async function doRefresh(): Promise<void> {
    perfMark('rooms:refresh:start')
    // The auth session this request belongs to. Anything that comes back after
    // it has ended belongs to a reader who is no longer here (WCR-03's
    // request-generation guard, applied to identity rather than to ordering).
    const generation = sessionGeneration
    try {
      const { data, error: apiError } = await api.GET('/v1/rooms')
      if (generation !== sessionGeneration) {
        return
      }
      // `data === undefined` is not redundant with the error check. A non-2xx
      // response whose body openapi-fetch cannot parse — an empty one, or a
      // proxy's error page — yields **neither** envelope: `{ data: undefined,
      // error: undefined }`. Testing only `apiError` then falls through to
      // `data.data` and throws `undefined is not an object`, which is what a
      // phone sees when the API is unreachable behind a dev proxy: the raw
      // TypeError lands in the error banner in place of a usable message.
      // `apiErrorMessage(undefined)` is already the right words for it.
      if (apiError !== undefined || data === undefined) {
        error.value = apiErrorMessage(apiError)
        return
      }
      const visibleRooms = applyLocalRoomMetadata(
        data.data.filter((room) => !locallyHiddenRooms.has(roomKey(room))),
      )
      // Wholesale replacement, not a merge into the cached set: a room left or
      // forgotten on another client has to disappear (guardrail 5's
      // prune-on-reconcile), and a restored list is exactly the case where
      // merging would keep one alive forever.
      rooms.value = visibleRooms
      syncUnreadCounts(visibleRooms)
      cacheRoomListTitles(visibleRooms)
      recomputeNewest(visibleRooms)
      error.value = null
      settled = true
      stale.value = false
      cache?.write(visibleRooms)
      // Fire-and-forget: titles arrive incrementally; the list re-renders as
      // the titles signal updates.
      void resolveUnnamedTitles(visibleRooms)
    } catch (cause) {
      if (generation === sessionGeneration) {
        error.value = cause instanceof Error ? cause.message : String(cause)
      }
    } finally {
      // Guarded rather than returned early: a `return` here would swallow an
      // exception still propagating out of the `try` (`no-unsafe-finally`).
      // A discarded completion must not clear the *new* session's loading
      // state, which would show an empty list as though it had loaded.
      if (generation === sessionGeneration) {
        loading.value = false
        perfMark('rooms:refresh:end', {
          ok: error.value === null,
          rooms: rooms.value.length,
        })
        // The race this phase exists to win is decided the first time the
        // network answers, so the boot summary is emitted here — once per
        // document, on failure as well, since a refresh that failed over
        // restored rows is itself a result worth reading (ADR 0085 phase 2).
        if (!summarised) {
          summarised = true
          perfMarkBootRoomList()
        }
      }
    }
  }

  /** Whether a refresh has ever completed successfully in this session. */
  let settled = false
  /** Bumped by `resetSession`; see `doRefresh`'s generation check. */
  let sessionGeneration = 0

  function resetSession(): void {
    // Always bumped, even when there is nothing to clear: a sign-out during
    // the very first refresh leaves the store empty but still has a response
    // in flight, and that response must not be applied to the next reader.
    sessionGeneration += 1
    if (
      rooms.value.length === 0 &&
      !settled &&
      !stale.value &&
      error.value === null &&
      loading.value
    ) {
      return
    }
    rooms.value = []
    syncUnreadCounts([])
    recomputeNewest([])
    requested.clear()
    requestedPreviews.clear()
    previewSlots.clear()
    members.clear()
    locallyHiddenRooms.clear()
    locallyCreatedRoomMetadata.clear()
    settled = false
    stale.value = false
    error.value = null
    loading.value = true
  }
  /** Whether this document's one-shot boot summary has been emitted. */
  let summarised = false

  /**
   * Paint the cached room list, if there is one and the network has not
   * already answered.
   *
   * Kicked off at construction rather than from a component, so the read is
   * already in flight before anything mounts — it is racing a ~1.3 s
   * server-side wait (ADR 0085's measurement) with a ~1 ms IndexedDB read, and
   * the only way to lose that race is to start late.
   */
  async function hydrate(): Promise<void> {
    // Same rule as `doRefresh`: rows belong to the session that asked for
    // them. A sign-out landing mid-read leaves exactly the pristine state
    // the guard below treats as safe to write into, so without this the
    // previous reader's cached rows would repaint the just-cleared list.
    const generation = sessionGeneration
    perfMark('rooms:cache:read:start')
    const cached = await cache?.read()
    perfMark('rooms:cache:read:end', {
      hit: cached !== undefined,
      rooms: cached?.length ?? 0,
    })
    if (cached === undefined || cached.length === 0) {
      return
    }
    // The network won the race, a mutation already populated the list, or the
    // session these rows belong to has ended. Cached rows are strictly older
    // than any of those, so they must not land on top.
    if (generation !== sessionGeneration || settled || rooms.value.length > 0) {
      return
    }
    const visible = cached.filter(
      (room) => !locallyHiddenRooms.has(roomKey(room)),
    )
    rooms.value = visible
    syncUnreadCounts(visible)
    recomputeNewest(visible)
    // Deliberately *not* `resolveUnnamedTitles`: that is a `/members` fetch per
    // unnamed room, and firing a burst of them at boot would spend exactly the
    // network the cache exists to avoid waiting on. DM titles paint from the
    // `axon.room_titles.v1` cache meanwhile, and the refresh resolves the rest.
    stale.value = true
    loading.value = false
    perfMark('rooms:hydrate', { rooms: visible.length })
  }

  void hydrate()

  /** In-flight refresh, shared: concurrent callers (mount + a burst of
   *  unknown-room frames) coalesce onto one request. */
  let refreshing: Promise<void> | null = null

  function refresh(): Promise<void> {
    refreshing ??= doRefresh().finally(() => {
      refreshing = null
    })
    return refreshing
  }

  function ensureLoaded(): Promise<void> {
    return settled ? Promise.resolve() : refresh()
  }

  async function membershipMutation(
    accountId: string,
    roomId: string,
    call: () => Promise<{ error?: unknown }>,
  ): Promise<RoomMembershipResult> {
    try {
      const { error: apiError } = await call()
      if (apiError !== undefined) {
        const message = apiErrorMessage(apiError)
        error.value = message
        return { ok: false, message }
      }
      const hiddenKey = hideRoomLocally(accountId, roomId)
      try {
        await refresh()
      } finally {
        locallyHiddenRooms.delete(hiddenKey)
      }
      return { ok: true }
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause)
      error.value = message
      return { ok: false, message }
    }
  }

  function leaveRoom(
    accountId: string,
    roomId: string,
  ): Promise<RoomMembershipResult> {
    return membershipMutation(accountId, roomId, () =>
      api.POST('/v1/accounts/{account_id}/rooms/{room_id}/leave', {
        params: { path: { account_id: accountId, room_id: roomId } },
      }),
    )
  }

  function forgetRoom(
    accountId: string,
    roomId: string,
  ): Promise<RoomMembershipResult> {
    return membershipMutation(accountId, roomId, () =>
      api.POST('/v1/accounts/{account_id}/rooms/{room_id}/forget', {
        params: { path: { account_id: accountId, room_id: roomId } },
      }),
    )
  }

  async function roomEntryMutation(
    call: () => Promise<{
      data?: { data: { room_id: string } }
      error?: unknown
    }>,
    onSuccess?: (roomId: string) => void,
  ): Promise<RoomEntryResult> {
    try {
      const { data, error: apiError } = await call()
      if (apiError !== undefined || data === undefined) {
        const message =
          apiError === undefined
            ? 'room entry failed'
            : apiErrorMessage(apiError)
        error.value = message
        return {
          ok: false,
          message,
          code: apiError === undefined ? null : apiErrorCode(apiError),
        }
      }
      onSuccess?.(data.data.room_id)
      await refresh()
      error.value = null
      return { ok: true, roomId: data.data.room_id }
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause)
      error.value = message
      return { ok: false, message, code: null }
    }
  }

  function joinRoom(
    accountId: string,
    roomIdOrAlias: string,
    serverNames: readonly string[] = [],
  ): Promise<RoomEntryResult> {
    return roomEntryMutation(() =>
      api.POST('/v1/accounts/{account_id}/rooms/join', {
        params: { path: { account_id: accountId } },
        body: {
          room_id_or_alias: roomIdOrAlias,
          server_names: [...serverNames],
        },
      }),
    )
  }

  function knockRoom(
    accountId: string,
    roomIdOrAlias: string,
    reason: string | null = null,
    serverNames: readonly string[] = [],
  ): Promise<RoomEntryResult> {
    return roomEntryMutation(() =>
      api.POST('/v1/accounts/{account_id}/rooms/knock', {
        params: { path: { account_id: accountId } },
        body: {
          room_id_or_alias: roomIdOrAlias,
          reason,
          server_names: [...serverNames],
        },
      }),
    )
  }

  function createRoom(
    accountId: string,
    request: CreateRoomRequest,
  ): Promise<RoomEntryResult> {
    return roomEntryMutation(
      () =>
        api.POST('/v1/accounts/{account_id}/rooms', {
          params: { path: { account_id: accountId } },
          body: request,
        }),
      (roomId) => {
        if (request.is_direct === true) {
          return
        }
        const name = emptyToNull(request.name)
        const topic = emptyToNull(request.topic)
        if (name === null && topic === null) {
          return
        }
        locallyCreatedRoomMetadata.set(
          roomKey({ account_id: accountId, room_id: roomId }),
          { name, topic },
        )
      },
    )
  }

  function createDm(
    accountId: string,
    userId: string,
  ): Promise<RoomEntryResult> {
    return (async () => {
      const existingRoomId = await existingDmRoomId(accountId, userId)
      if (existingRoomId !== null) {
        error.value = null
        return { ok: true, roomId: existingRoomId }
      }
      return roomEntryMutation(() =>
        api.POST('/v1/accounts/{account_id}/rooms/dm', {
          params: { path: { account_id: accountId } },
          body: { user_id: userId },
        }),
      )
    })()
  }

  async function existingDmRoomId(
    accountId: string,
    userId: string,
  ): Promise<string | null> {
    if (loading.value) {
      await refresh()
    }
    const candidates = rooms.value.filter(
      (room) => room.account_id === accountId && isLikelyDm(room),
    )
    const matches = await Promise.all(
      candidates.map(async (room) =>
        isOneToOneDmRoster(room.account_user_id ?? null, userId, [
          ...(await roomMembers(room)),
        ])
          ? room.room_id
          : null,
      ),
    )
    return matches.find((roomId): roomId is string => roomId !== null) ?? null
  }

  function hideRoomLocally(accountId: string, roomId: string): string {
    const key = roomKey({ account_id: accountId, room_id: roomId })
    locallyHiddenRooms.add(key)
    const next = rooms.value.filter((room) => roomKey(room) !== key)
    if (next.length !== rooms.value.length) {
      rooms.value = next
      syncUnreadCounts(next)
      recomputeNewest(next)
    }
    return key
  }

  function noteActivityRoom(
    accountId: string,
    roomId: string,
    ts: number,
    eventId?: string,
  ): RoomDto | null {
    const key = roomKey({ account_id: accountId, room_id: roomId })
    const current = rooms.value
    const index = current.findIndex(
      (room) => room.account_id === accountId && room.room_id === roomId,
    )
    if (index === -1) {
      // A room we don't know: freshly joined (or the list predates it).
      // Re-read the list; the in-flight guard coalesces frame bursts.
      void refresh()
      return null
    }
    const room = current[index]
    // `<`, not `<=`: an event whose origin_ts ties the recorded activity (a
    // same-millisecond burst, or a frame racing a refresh that already
    // recorded its ts) must still update the preview/last_event_id.
    if (ts < room.last_activity_ts) {
      return null
    }
    if (key === newestKey) {
      // Already the freshest row: the sort order cannot change, so mutate in
      // place without a signal write — a burst in the active room must not
      // re-render and re-sort the whole list per message (the room-list-perf
      // rule). The row's relative-time label catches up on the next render.
      room.last_activity_ts = ts
      if (eventId !== undefined) {
        room.last_event_id = eventId
      }
      newestTs = ts
      return room
    }
    const nextRoom =
      eventId === undefined
        ? { ...room, last_activity_ts: ts }
        : { ...room, last_activity_ts: ts, last_event_id: eventId }
    rooms.value = current.map((r, i) => (i === index ? nextRoom : r))
    if (ts > newestTs) {
      newestTs = ts
      newestKey = key
    }
    return nextRoom
  }

  function noteActivity(accountId: string, roomId: string, ts: number): void {
    noteActivityRoom(accountId, roomId, ts)
  }

  function noteTimelineEvent(event: EventDto): void {
    const room = noteActivityRoom(
      event.account_id,
      event.room_id,
      event.origin_ts,
      event.event_id,
    )
    if (room !== null) {
      setLivePreview(room, event)
    }
  }

  function noteUnreadCounts(
    accountId: string,
    roomId: string,
    notificationCount: number,
    highlightCount: number,
  ): void {
    const key = roomKey({ account_id: accountId, room_id: roomId })
    const known = rooms.value.some(
      (room) => room.account_id === accountId && room.room_id === roomId,
    )
    if (!known) {
      void refresh()
    }
    setUnreadCounts(
      key,
      countFromRoom(notificationCount),
      countFromRoom(highlightCount),
    )
  }

  return {
    rooms: computed(() => rooms.value),
    unreadKeys: computed(() => unreadKeys.value),
    unreadTotal: computed(() => unreadTotal.value),
    loading: computed(() => loading.value),
    stale: computed(() => stale.value),
    error,
    titles: computed(() => titles.value),
    refresh,
    ensureLoaded,
    resetSession,
    leaveRoom,
    forgetRoom,
    joinRoom,
    knockRoom,
    createRoom,
    createDm,
    preview: (key) => previewSlot(key).value,
    unreadCount: (key) => unreadSlot(key).value.notificationCount,
    hydratePreview,
    noteActivity,
    noteTimelineEvent,
    noteUnreadCounts,
  }
}

function countFromRoom(value: number | null | undefined): number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value > 0
    ? value
    : 0
}

function isOneToOneDmRoster(
  ownUserId: string | null,
  targetUserId: string,
  members: readonly MemberDto[],
): boolean {
  const active = members.filter(
    (member) => member.membership === 'join' || member.membership === 'invite',
  )
  if (!active.some((member) => member.user_id === targetUserId)) {
    return false
  }
  if (
    ownUserId !== null &&
    !active.some((member) => member.user_id === ownUserId)
  ) {
    return false
  }
  return active.every(
    (member) =>
      member.user_id === targetUserId ||
      ownUserId === null ||
      member.user_id === ownUserId,
  )
}

function loadTitleCache(storage: Storage): ReadonlyMap<string, string> {
  const raw = storage.getItem(ROOM_TITLE_CACHE_KEY)
  if (raw === null) {
    return new Map()
  }
  try {
    const parsed = JSON.parse(raw) as unknown
    if (
      parsed === null ||
      typeof parsed !== 'object' ||
      !Array.isArray((parsed as { titles?: unknown }).titles)
    ) {
      return new Map()
    }
    const entries = (parsed as { titles: unknown[] }).titles.flatMap(
      (entry): [string, string][] => {
        if (
          Array.isArray(entry) &&
          entry.length === 2 &&
          typeof entry[0] === 'string' &&
          typeof entry[1] === 'string' &&
          entry[1].trim() !== ''
        ) {
          return [[entry[0], entry[1].trim()]]
        }
        return []
      },
    )
    return new Map(entries)
  } catch {
    return new Map()
  }
}

function saveTitleCache(
  storage: Storage,
  titleCache: ReadonlyMap<string, string>,
): void {
  try {
    storage.setItem(
      ROOM_TITLE_CACHE_KEY,
      JSON.stringify({ version: 1, titles: [...titleCache] }),
    )
  } catch {
    // Quota or storage-denied: the cache is an optimization — losing a write
    // must not reject the background title resolution (WCR-02) or paint an
    // error banner over a successfully loaded room list.
  }
}

function blankRoomField(value: string | null | undefined): boolean {
  return value === null || value === undefined || value.trim() === ''
}

function emptyToNull(value: string | null | undefined): string | null {
  if (value === null || value === undefined) {
    return null
  }
  const trimmed = value.trim()
  return trimmed === '' ? null : trimmed
}

/**
 * A message the preview can actually show. A redacted or bodiless row (a
 * removed message, an image) is *skipped*, not previewed as blank: `body` is
 * null there, and `body?.trim() !== ''` would wave it through as the newest
 * message and leave the room with no preview at all.
 */
function isPreviewEvent(event: EventDto): boolean {
  return (
    event.type === 'm.room.message' &&
    event.state_key == null &&
    !event.redacted &&
    typeof event.body === 'string' &&
    event.body.trim() !== ''
  )
}

function previewBody(body: string | null | undefined): string | undefined {
  if (body === null || body === undefined) {
    return undefined
  }
  const text = markdownToPlainText(body)
  return text === '' ? undefined : text
}
