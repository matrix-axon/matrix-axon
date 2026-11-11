/**
 * The typed room-detail reads behind Room Information (ADR 0133 decision 1):
 * `/metadata`, `/homeserver_details` and `/info`, for the one room the panel
 * shows.
 *
 * Freshness story (AGENTS.md rule 8, ADR 0133 decision 2): each read repeats
 * only on a `room_details.changed` frame for this account and room naming its
 * source, and every read the store holds repeats on reconnect and when the
 * panel is opened again. Nothing polls: the server publishes a frame for every
 * change it stores and at the end of every background job a read saw queued or
 * running, so an open panel learns of each answer without asking.
 *
 * One thing changes without the server saying so: time. An answer's
 * `expires_at` or a backoff's `next_attempt_at` passing changes its label
 * ("may be out of date", "Unknown (couldn't check)" becoming "Unknown (not
 * cached)"), but stores nothing, so no frame follows. The state therefore
 * carries the presentation clock, `now`, which a one-shot timer moves forward
 * at the next such deadline in the answers held — a local re-render, never a
 * request. A page coming back into view moves it too, because a suspended
 * device (iOS especially) does not run that timer while asleep. And because a
 * laptop that sleeps with the tab in view gets no `visibilitychange`, while its
 * timers lose the time asleep, the timer never waits more than a minute before
 * comparing the wall clock with the deadline; it changes the state only once a
 * deadline has actually passed.
 *
 * For a space it also pages the children from `space/hierarchy` (decision 6),
 * asks the server to look up the unjoined ones (`enrich`, decision 7), and
 * holds one unjoined room's `/discovery` preview (decision 8). The hierarchy
 * re-reads on a `space_hierarchy` frame for the space, on an `m.space.child`
 * timeline event in it (a link change, which no crawl frame announces), and
 * on reconnect; the preview re-reads on a `discovery` frame for its room, on
 * reconnect, and, while the server cannot summarise rooms directly and the
 * preview has no unexpired answer, on any `space_hierarchy` frame for the
 * account (decision 2). Two timers exist, and neither polls: the clock timer
 * below, which reads nothing, and decision 7's crawl-retry timer, which sends
 * one read per transient crawl failure the server reports.
 *
 * `/pinned`, `/space/parents`, `/upgrade` and `/power_levels` stay in the
 * panel's own loader; they are not part of issue 618.
 */
import { effect, signal, untracked, type ReadonlySignal } from '@preact/signals'
import { apiErrorMessage, type ApiClient } from '../api/client'
import {
  roomDetailsChange,
  timelineEvent,
  type RoomDetailSource,
} from '../api/frames'
import type { components } from '../api/schema'
import type { LiveConnection } from './live-connection'
import {
  appendPage,
  crawlPaused,
  crawlRetryable,
  HIERARCHY_PAGE,
  listFromPage,
  mergeReread,
  rereadLimit,
  type HierarchyList,
  type RereadMode,
  type SpaceHierarchyPage,
} from './space-hierarchy'

type RoomMetadataDto = components['schemas']['RoomMetadataDto']
type RoomHomeserverDetailsDto =
  components['schemas']['RoomHomeserverDetailsDto']
type RoomInfoDto = components['schemas']['RoomInfoDto']
type RoomDiscoveryDto = components['schemas']['RoomDiscoveryDto']
type DiscoveredRoomDto = components['schemas']['DiscoveredRoomDto']

/** The three reads this store owns. */
export type RoomDetailsRead = 'metadata' | 'homeserverDetails' | 'info'

/** Which read a frame source asks this store to repeat; others are not its. */
const READ_FOR_SOURCE: Partial<Record<RoomDetailSource, RoomDetailsRead>> = {
  metadata: 'metadata',
  member_counts: 'info',
  homeserver_details: 'homeserverDetails',
}

export interface RoomDetailsSlot<T> {
  /** The latest response, kept through a failed re-read. */
  value: T | null
  /** Why the latest read failed, or `null` after one succeeds. */
  error: string | null
  /** Whether the store has asked for this read for the shown room at all. */
  requested: boolean
}

export interface HierarchySlot extends RoomDetailsSlot<HierarchyList> {
  /** A "Load more" read is in flight (or queued behind a re-read). */
  loadingMore: boolean
}

export interface RoomDetailsState {
  /** `roomDetailsKey` of the room the slots describe, or `null` for none. */
  key: string | null
  metadata: RoomDetailsSlot<RoomMetadataDto>
  homeserverDetails: RoomDetailsSlot<RoomHomeserverDetailsDto>
  info: RoomDetailsSlot<RoomInfoDto>
  /** The space's loaded children; never requested for a room that is not a space. */
  hierarchy: HierarchySlot
  /**
   * The client time labels are judged at: server timestamps in the answers
   * (`expires_at`, `next_attempt_at`) are compared against it. Moved forward
   * by every answer, by a timer at the next deadline the answers name, and when
   * the page becomes visible again.
   */
  now: number
}

export interface RoomDetailsTarget {
  accountId: string
  roomId: string
  /**
   * Whether the account has joined the room. `/homeserver_details` is read
   * only for joined rooms (decision 5): the server acquires nothing for any
   * other room, so the read could only ever say "unknown".
   */
  joined: boolean
  /**
   * Whether the room is a space (`room_type` `m.space`), whose children the
   * panel shows from `space/hierarchy`. Absent is not a space.
   */
  space?: boolean
}

/** The one unjoined room whose summary a join prompt previews (decision 8). */
export interface RoomPreviewTarget {
  accountId: string
  roomId: string
  /**
   * The summary the prompt was opened from, such as a hierarchy row's
   * `discovered`, shown until `/discovery` answers.
   */
  seed?: DiscoveredRoomDto | null
}

export interface RoomPreviewState {
  /** `roomDetailsKey` of the previewed room, or `null` for none. */
  key: string | null
  seed: DiscoveredRoomDto | null
  discovery: RoomDetailsSlot<RoomDiscoveryDto>
}

export interface RoomDetailsStore {
  state: ReadonlySignal<RoomDetailsState>
  /** The open join prompt's preview; independent of the panel's room. */
  preview: ReadonlySignal<RoomPreviewState>
  /**
   * Show `target`'s details, or none. A new room aborts the previous room's
   * reads and starts its own; the same room again only adds a read that has
   * become due (`/homeserver_details` once the room is known to be joined).
   * Showing `null` then the same room again is a reopen, and re-reads.
   */
  show(target: RoomDetailsTarget | null): void
  /**
   * Read the shown space's next page of children ("Load more"), never with
   * `enrich`. Queued behind a re-read in flight; a no-op at the end.
   */
  loadMore(): void
  /**
   * Repeat every read whose latest attempt failed, for the room shown. The
   * panel's Retry: a failure otherwise waits for a frame, a reconnect or a
   * reopen.
   */
  retry(): void
  /**
   * Preview `target` with one `/discovery` read, replacing any other preview:
   * no surface previews more than one room at a time (decision 8). The same
   * room again keeps its answer and only updates the seed.
   */
  showPreview(target: RoomPreviewTarget): void
  /**
   * Stop previewing `target`, aborting its read. A no-op when another room
   * has since taken the preview, so one prompt closing never clears another's.
   */
  closePreview(target: RoomPreviewTarget): void
  /**
   * Repeat the preview's `/discovery` read after it failed: the prompt's
   * Retry. A failure otherwise waits for a frame, a reconnect or a reopen.
   */
  retryPreview(): void
  /**
   * Stop listening: the frame subscription, the reconnect effect, the
   * `visibilitychange` listener and the clock timer, and abort the shown
   * room's reads. The app graph keeps the store for the page's life; tests
   * and anything else that builds a store for less call this.
   */
  dispose(): void
}

/** A composite in-memory key (`'\0'`-joined; `clients/web/AGENTS.md`). */
export function roomDetailsKey(accountId: string, roomId: string): string {
  return `${accountId}\0${roomId}`
}

function emptySlot<T>(): RoomDetailsSlot<T> {
  return { value: null, error: null, requested: false }
}

function emptyHierarchy(): HierarchySlot {
  return { ...emptySlot<HierarchyList>(), loadingMore: false }
}

export function emptyRoomPreview(key: string | null = null): RoomPreviewState {
  return { key, seed: null, discovery: emptySlot() }
}

export function emptyRoomDetails(
  key: string | null,
  now = 0,
): RoomDetailsState {
  return {
    key,
    metadata: emptySlot(),
    homeserverDetails: emptySlot(),
    info: emptySlot(),
    hierarchy: emptyHierarchy(),
    now,
  }
}

/** The crawl-retry timer's bounds (decision 7): the longest server backoff, and its floor. */
export const CRAWL_RETRY_MIN_MS = 5_000
export const CRAWL_RETRY_REARM_MIN_MS = 10_000
export const CRAWL_RETRY_MAX_MS = 30 * 60_000

/**
 * The longest the clock timer waits before comparing the wall clock with the
 * next deadline. Timers lose the time a sleeping machine spends asleep, so a
 * deadline timer alone could fire hours late; this bounds how late a label
 * can be to a minute. It reads no network and writes no state until a
 * deadline passes.
 */
const CLOCK_RECHECK_MS = 60_000

/**
 * The earliest server timestamp after `now` at which a label in `state`
 * changes, or `null` when none will. These are exactly the comparisons the
 * formatter makes (`room-details-format.ts`): a homeserver detail's
 * `expires_at` (the staleness note) and `refresh.next_attempt_at` (a held
 * refresh), and a snapshot's `absence_check.next_attempt_at` (a held check).
 */
export function nextLabelDeadline(
  state: RoomDetailsState,
  now: number,
): number | null {
  const deadlines: Array<number | null | undefined> = []
  const homeserver = state.homeserverDetails.value
  if (homeserver !== null) {
    for (const detail of [
      homeserver.local_aliases,
      homeserver.directory_visibility,
    ]) {
      deadlines.push(detail.expires_at, detail.refresh.next_attempt_at)
    }
  }
  const metadata = state.metadata.value
  if (metadata !== null) {
    for (const snapshot of Object.values(metadata)) {
      deadlines.push(snapshot.absence_check?.next_attempt_at)
    }
  }
  let next: number | null = null
  for (const deadline of deadlines) {
    if (typeof deadline === 'number' && deadline >= now) {
      next = next === null ? deadline : Math.min(next, deadline)
    }
  }
  return next
}

/**
 * Why a hierarchy re-read from the start was sent, which decides what its
 * response may prompt (decision 7): only a re-read prompted by a frame or a
 * reconnect, without `enrich`, resumes a paused crawl, and only the retry
 * timer's own read doubles the timer.
 */
type RereadOrigin =
  'open' | 'frame' | 'reconnect' | 'resume' | 'timer' | 'retry'

interface Reread {
  mode: RereadMode
  enrich: boolean
  origin: RereadOrigin
}

/** Fold a second re-read request into one already waiting. */
function mergeRereads(waiting: Reread | null, next: Reread): Reread {
  if (waiting === null) return next
  return {
    mode:
      waiting.mode === 'reconnect' || next.mode === 'reconnect'
        ? 'reconnect'
        : 'frame',
    enrich: waiting.enrich || next.enrich,
    origin: next.origin === 'timer' ? 'timer' : waiting.origin,
  }
}

/** The shown space's hierarchy reads: one at a time, re-reads before "Load more". */
interface HierarchySession {
  inFlight: boolean
  pendingReread: Reread | null
  pendingMore: boolean
  /**
   * The open read's `enrich` has not reached the server yet: the first read
   * failed, or the room became a joined space after the panel opened. Every
   * re-read carries it until one succeeds.
   */
  openEnrich: boolean
  /** What to repeat on Retry after a failed read. */
  failed: Reread | 'more' | null
  retryTimer: ReturnType<typeof setTimeout> | null
  /** The last timer delay, doubled when the server declines the timer's read. */
  retryDelay: number
  /** The `next_attempt_at` the armed (or last fired) timer was aimed at. */
  retryTarget: number | null
}

/** One shown room: its identity, its reads' abort handle, and their bookkeeping. */
interface Session {
  key: string
  target: RoomDetailsTarget
  controller: AbortController
  inFlight: Set<RoomDetailsRead>
  /** A frame arrived mid-read: exactly one more read follows the current one. */
  dirty: Set<RoomDetailsRead>
  hierarchy: HierarchySession
}

/** The previewed room's `/discovery` read. */
interface PreviewSession {
  key: string
  target: RoomPreviewTarget
  controller: AbortController
  inFlight: boolean
  /** A `discovery` frame arrived mid-read: exactly one more read follows. */
  dirty: boolean
  /**
   * A `space_hierarchy` frame arrived mid-read. Whether it calls for a
   * fallback re-read depends on `support` and `discovered`, which the read in
   * flight is about to answer, so it is judged against that answer rather
   * than the one held when the frame came: the fallback crawl can end, and
   * publish its only frame, before the first answer reaches the browser.
   */
  hierarchyDuringRead: boolean
}

type ReadResult<T> = { ok: true; value: T } | { ok: false; message: string }

export function createRoomDetailsStore(
  api: ApiClient,
  live: LiveConnection,
): RoomDetailsStore {
  const state = signal<RoomDetailsState>(emptyRoomDetails(null))
  let session: Session | null = null
  let clockTimer: ReturnType<typeof setTimeout> | null = null

  function stopClock(): void {
    if (clockTimer !== null) {
      clearTimeout(clockTimer)
      clockTimer = null
    }
  }

  /** Arm one timer for the next deadline the held answers name, if any. */
  function armClock(): void {
    stopClock()
    if (session === null) return
    const next = nextLabelDeadline(state.value, state.value.now)
    if (next === null) return
    // A label compares `deadline < now`, so it changes just after the deadline.
    const wait = Math.max(0, next - Date.now()) + 1
    clockTimer = setTimeout(tick, Math.min(wait, CLOCK_RECHECK_MS))
  }

  /**
   * Move the presentation clock to the current time if a deadline has passed
   * since it last moved, then re-arm. A check that finds nothing due changes
   * nothing, so it renders nothing.
   */
  function tick(): void {
    clockTimer = null
    if (session === null) return
    const now = Date.now()
    const next = nextLabelDeadline(state.value, state.value.now)
    if (next !== null && next < now) {
      state.value = { ...state.value, now }
    }
    armClock()
  }

  const onVisibilityChange = () => {
    if (document.visibilityState === 'visible') tick()
  }
  if (typeof document !== 'undefined') {
    document.addEventListener('visibilitychange', onVisibilityChange)
  }

  async function fetchRead(
    read: RoomDetailsRead,
    target: RoomDetailsTarget,
    abort: AbortSignal,
  ): Promise<ReadResult<unknown>> {
    const options = {
      params: {
        path: { account_id: target.accountId, room_id: target.roomId },
      },
      signal: abort,
    }
    try {
      const { data, error } =
        read === 'metadata'
          ? await api.GET(
              '/v1/accounts/{account_id}/rooms/{room_id}/metadata',
              options,
            )
          : read === 'homeserverDetails'
            ? await api.GET(
                '/v1/accounts/{account_id}/rooms/{room_id}/homeserver_details',
                options,
              )
            : await api.GET(
                '/v1/accounts/{account_id}/rooms/{room_id}/info',
                options,
              )
      if (data === undefined) {
        return { ok: false, message: apiErrorMessage(error) }
      }
      return { ok: true, value: data.data }
    } catch (cause) {
      return {
        ok: false,
        message: cause instanceof Error ? cause.message : String(cause),
      }
    }
  }

  function read(owner: Session, which: RoomDetailsRead): void {
    if (session !== owner) return
    if (owner.inFlight.has(which)) {
      owner.dirty.add(which)
      return
    }
    owner.inFlight.add(which)
    if (!state.value[which].requested) {
      state.value = {
        ...state.value,
        [which]: { ...state.value[which], requested: true },
      }
    }
    void fetchRead(which, owner.target, owner.controller.signal).then(
      (result) => {
        // A late answer for a room the panel has left (or a closed panel) is
        // dropped; its request was aborted, but a settled one can still land.
        if (session !== owner) return
        owner.inFlight.delete(which)
        // The room was left mid-read: its homeserver answer no longer applies.
        if (which === 'homeserverDetails' && !owner.target.joined) {
          owner.dirty.delete(which)
          return
        }
        const slot = state.value[which]
        state.value = {
          ...state.value,
          [which]: result.ok
            ? { value: result.value, error: null, requested: true }
            : { ...slot, error: result.message },
          now: Date.now(),
        }
        armClock()
        if (owner.dirty.delete(which)) read(owner, which)
      },
    )
  }

  function hierarchyOptions(
    target: RoomDetailsTarget,
    query: { limit: number; cursor?: string; enrich?: true },
    abort: AbortSignal,
  ) {
    return {
      params: {
        path: { account_id: target.accountId, room_id: target.roomId },
        query,
      },
      signal: abort,
    }
  }

  async function fetchHierarchy(
    target: RoomDetailsTarget,
    query: { limit: number; cursor?: string; enrich?: true },
    abort: AbortSignal,
  ): Promise<ReadResult<SpaceHierarchyPage>> {
    try {
      const { data, error } = await api.GET(
        '/v1/accounts/{account_id}/rooms/{room_id}/space/hierarchy',
        hierarchyOptions(target, query, abort),
      )
      if (data === undefined) {
        return { ok: false, message: apiErrorMessage(error) }
      }
      return { ok: true, value: data.data }
    } catch (cause) {
      return {
        ok: false,
        message: cause instanceof Error ? cause.message : String(cause),
      }
    }
  }

  function setHierarchy(patch: Partial<HierarchySlot>): void {
    state.value = {
      ...state.value,
      hierarchy: { ...state.value.hierarchy, ...patch },
    }
  }

  /** Whether `enrich` may be sent for this room at all (decision 7). */
  function enrichable(target: RoomDetailsTarget): boolean {
    return target.joined && target.space === true
  }

  function stopCrawlRetry(owner: Session): void {
    const h = owner.hierarchy
    if (h.retryTimer !== null) {
      clearTimeout(h.retryTimer)
      h.retryTimer = null
    }
  }

  /**
   * Decision 7's transient-failure timer, after a response shows progress
   * row 5. One timer at a time; its delay is `next_attempt_at` minus the
   * client clock, within 5 s and 30 min. When the timer's own read finds the
   * crawl still waiting on the same `next_attempt_at` (a fast client clock),
   * it is armed again at twice the previous delay, at least 10 s.
   */
  function afterCrawlStatus(
    owner: Session,
    list: HierarchyList,
    origin: RereadOrigin | 'more',
  ): void {
    const h = owner.hierarchy
    const discovery = list.discovery
    if (!enrichable(owner.target) || !crawlRetryable(discovery)) {
      stopCrawlRetry(owner)
      h.retryDelay = 0
      h.retryTarget = null
      return
    }
    if (h.retryTimer !== null) return
    const target = discovery.next_attempt_at ?? null
    const declined =
      origin === 'timer' && target !== null && target === h.retryTarget
    const delay = declined
      ? Math.max(CRAWL_RETRY_REARM_MIN_MS, h.retryDelay * 2)
      : (target ?? 0) - Date.now()
    h.retryDelay = Math.min(
      CRAWL_RETRY_MAX_MS,
      Math.max(CRAWL_RETRY_MIN_MS, delay),
    )
    h.retryTarget = target
    h.retryTimer = setTimeout(() => {
      h.retryTimer = null
      reread(owner, { mode: 'frame', enrich: true, origin: 'timer' })
    }, h.retryDelay)
  }

  /** Send the next queued hierarchy read, re-reads first. */
  function drainHierarchy(owner: Session): void {
    const h = owner.hierarchy
    if (session !== owner || h.inFlight) return
    if (h.pendingReread !== null) {
      const next = h.pendingReread
      h.pendingReread = null
      void sendReread(owner, next)
      return
    }
    if (h.pendingMore) {
      h.pendingMore = false
      void sendMore(owner)
    }
  }

  /**
   * Re-read the shown space's children from the start (decision 6), or queue
   * the request behind the read in flight; queued requests fold into one.
   */
  function reread(owner: Session, request: Reread): void {
    if (session !== owner || owner.target.space !== true) return
    const h = owner.hierarchy
    h.pendingReread = mergeRereads(h.pendingReread, {
      ...request,
      enrich: request.enrich && enrichable(owner.target),
    })
    drainHierarchy(owner)
  }

  async function sendReread(owner: Session, request: Reread): Promise<void> {
    const h = owner.hierarchy
    h.inFlight = true
    const enrich = request.enrich || h.openEnrich
    if (!state.value.hierarchy.requested) setHierarchy({ requested: true })
    const result = await fetchHierarchy(
      owner.target,
      {
        limit: rereadLimit(state.value.hierarchy.value),
        ...(enrich ? { enrich: true as const } : {}),
      },
      owner.controller.signal,
    )
    if (session !== owner) return
    h.inFlight = false
    if (!result.ok) {
      h.failed = request
      setHierarchy({ error: result.message })
      drainHierarchy(owner)
      return
    }
    if (enrich) h.openEnrich = false
    h.failed = null
    const list = mergeReread(
      state.value.hierarchy.value,
      result.value,
      request.mode,
    )
    setHierarchy({ value: list, error: null, requested: true })
    afterCrawlStatus(owner, list, request.origin)
    // A paused crawl stays `in_progress` with nothing running until an
    // `enrich` read resumes it (decision 7). Only a plain re-read a frame or
    // a reconnect prompted sends that read, so its response never prompts
    // another.
    if (
      !enrich &&
      (request.origin === 'frame' || request.origin === 'reconnect') &&
      enrichable(owner.target) &&
      crawlPaused(list.discovery)
    ) {
      reread(owner, { mode: 'frame', enrich: true, origin: 'resume' })
    }
    drainHierarchy(owner)
  }

  async function sendMore(owner: Session): Promise<void> {
    const h = owner.hierarchy
    const list = state.value.hierarchy.value
    if (list === null || list.nextCursor === null) {
      setHierarchy({ loadingMore: false })
      drainHierarchy(owner)
      return
    }
    h.inFlight = true
    const result = await fetchHierarchy(
      owner.target,
      { limit: HIERARCHY_PAGE, cursor: list.nextCursor },
      owner.controller.signal,
    )
    if (session !== owner) return
    h.inFlight = false
    if (!result.ok) {
      h.failed = 'more'
      setHierarchy({ error: result.message, loadingMore: false })
      drainHierarchy(owner)
      return
    }
    h.failed = null
    // A re-read since this page was asked for may have moved the list's
    // cursor: append to the list as it is now, de-duplicated.
    const current = state.value.hierarchy.value
    const next =
      current === null
        ? listFromPage(result.value)
        : appendPage(current, result.value)
    setHierarchy({ value: next, error: null, loadingMore: false })
    afterCrawlStatus(owner, next, 'more')
    drainHierarchy(owner)
  }

  function loadMore(): void {
    const owner = session
    if (owner === null || owner.target.space !== true) return
    const list = state.value.hierarchy.value
    if (list === null || list.nextCursor === null) return
    if (owner.hierarchy.pendingMore || state.value.hierarchy.loadingMore) {
      return
    }
    owner.hierarchy.pendingMore = true
    setHierarchy({ loadingMore: true })
    drainHierarchy(owner)
  }

  function show(target: RoomDetailsTarget | null): void {
    const key =
      target === null ? null : roomDetailsKey(target.accountId, target.roomId)
    if (session !== null && key === session.key && target !== null) {
      const becameJoined = target.joined && !session.target.joined
      const left = !target.joined && session.target.joined
      const becameSpace = target.space === true && session.target.space !== true
      const wasEnrichable = enrichable(session.target)
      session.target = target
      if (becameJoined) read(session, 'homeserverDetails')
      if (!wasEnrichable && enrichable(target)) {
        // The panel now shows a joined space's children: that is the open
        // that asks for enrichment, whenever it is first true.
        session.hierarchy.openEnrich = true
      }
      if (!enrichable(target)) {
        session.hierarchy.openEnrich = false
        stopCrawlRetry(session)
      }
      if (becameSpace) {
        reread(session, { mode: 'reconnect', enrich: false, origin: 'open' })
      } else if (!wasEnrichable && enrichable(target)) {
        reread(session, { mode: 'frame', enrich: true, origin: 'open' })
      }
      if (left) {
        // Frames for it are ignored from now on, so a held answer would sit
        // unrefreshed with no note. Forget it: the row reads "not cached",
        // as it does for any room the account has not joined.
        session.dirty.delete('homeserverDetails')
        state.value = {
          ...state.value,
          homeserverDetails: { value: null, error: null, requested: false },
        }
        armClock()
      }
      return
    }
    if (session !== null) {
      session.controller.abort()
      stopCrawlRetry(session)
    }
    session = null
    stopClock()
    state.value = emptyRoomDetails(key, Date.now())
    if (target === null || key === null) return
    const owner: Session = {
      key,
      target,
      controller: new AbortController(),
      inFlight: new Set(),
      dirty: new Set(),
      hierarchy: {
        inFlight: false,
        pendingReread: null,
        pendingMore: false,
        openEnrich: enrichable(target),
        failed: null,
        retryTimer: null,
        retryDelay: 0,
        retryTarget: null,
      },
    }
    session = owner
    read(owner, 'metadata')
    read(owner, 'info')
    if (target.joined) read(owner, 'homeserverDetails')
    reread(owner, { mode: 'reconnect', enrich: false, origin: 'open' })
  }

  const preview = signal<RoomPreviewState>(emptyRoomPreview())
  let previewSession: PreviewSession | null = null

  async function fetchDiscovery(
    target: RoomPreviewTarget,
    abort: AbortSignal,
  ): Promise<ReadResult<RoomDiscoveryDto>> {
    try {
      const { data, error } = await api.GET(
        '/v1/accounts/{account_id}/rooms/{room_id}/discovery',
        {
          params: {
            path: { account_id: target.accountId, room_id: target.roomId },
          },
          signal: abort,
        },
      )
      if (data === undefined) {
        return { ok: false, message: apiErrorMessage(error) }
      }
      return { ok: true, value: data.data }
    } catch (cause) {
      return {
        ok: false,
        message: cause instanceof Error ? cause.message : String(cause),
      }
    }
  }

  function readPreview(owner: PreviewSession): void {
    if (previewSession !== owner) return
    if (owner.inFlight) {
      owner.dirty = true
      return
    }
    owner.inFlight = true
    if (!preview.value.discovery.requested) {
      preview.value = {
        ...preview.value,
        discovery: { ...preview.value.discovery, requested: true },
      }
    }
    void fetchDiscovery(owner.target, owner.controller.signal).then(
      (result) => {
        if (previewSession !== owner) return
        owner.inFlight = false
        const slot = preview.value.discovery
        preview.value = {
          ...preview.value,
          discovery: result.ok
            ? { value: result.value, error: null, requested: true }
            : { ...slot, error: result.message },
        }
        const again =
          owner.dirty || (owner.hierarchyDuringRead && previewAwaitsFallback())
        owner.dirty = false
        owner.hierarchyDuringRead = false
        if (again) readPreview(owner)
      },
    )
  }

  function retryPreview(): void {
    const owner = previewSession
    if (owner === null || preview.value.discovery.error === null) return
    // Back to "pending" at once, so the prompt does not offer Retry again
    // while this one is in flight.
    preview.value = {
      ...preview.value,
      discovery: { ...preview.value.discovery, error: null },
    }
    readPreview(owner)
  }

  /**
   * Whether the preview still waits for an answer the unsupported fallback
   * would bring (decision 2): the server cannot summarise rooms directly, and
   * there is no unexpired `discovered` of any status. Every status is a
   * conclusive answer with its own expiry, so once one arrives, frames stop.
   */
  function previewAwaitsFallback(): boolean {
    const discovery = preview.value.discovery.value
    if (discovery === null || discovery.support !== 'unsupported') return false
    const discovered = discovery.discovered ?? null
    return discovered === null || discovered.expires_at < Date.now()
  }

  function showPreview(target: RoomPreviewTarget): void {
    const key = roomDetailsKey(target.accountId, target.roomId)
    if (previewSession !== null && previewSession.key === key) {
      previewSession.target = target
      preview.value = { ...preview.value, seed: target.seed ?? null }
      return
    }
    previewSession?.controller.abort()
    preview.value = { ...emptyRoomPreview(key), seed: target.seed ?? null }
    const owner: PreviewSession = {
      key,
      target,
      controller: new AbortController(),
      inFlight: false,
      dirty: false,
      hierarchyDuringRead: false,
    }
    previewSession = owner
    readPreview(owner)
  }

  function closePreview(target: RoomPreviewTarget): void {
    const key = roomDetailsKey(target.accountId, target.roomId)
    if (previewSession === null || previewSession.key !== key) return
    previewSession.controller.abort()
    previewSession = null
    preview.value = emptyRoomPreview()
  }

  const unsubscribe = live.subscribe((frame) => {
    const owner = session
    const event = timelineEvent(frame)
    if (event !== null) {
      // A link added to or removed from the shown space: no crawl frame
      // announces it, so the timeline does.
      if (
        owner !== null &&
        event.type === 'm.space.child' &&
        event.account_id === owner.target.accountId &&
        event.room_id === owner.target.roomId
      ) {
        reread(owner, { mode: 'frame', enrich: false, origin: 'frame' })
      }
      return
    }
    const change = roomDetailsChange(frame)
    if (change === null) return
    const previewOwner = previewSession
    if (
      previewOwner !== null &&
      frame.accountId === previewOwner.target.accountId
    ) {
      if (
        change.roomId === previewOwner.target.roomId &&
        change.sources.has('discovery')
      ) {
        readPreview(previewOwner)
      } else if (change.sources.has('space_hierarchy')) {
        if (previewOwner.inFlight) previewOwner.hierarchyDuringRead = true
        else if (previewAwaitsFallback()) readPreview(previewOwner)
      }
    }
    if (
      owner === null ||
      frame.accountId !== owner.target.accountId ||
      change.roomId !== owner.target.roomId
    ) {
      return
    }
    for (const source of change.sources) {
      if (source === 'space_hierarchy') {
        reread(owner, { mode: 'frame', enrich: false, origin: 'frame' })
        continue
      }
      const which = READ_FOR_SOURCE[source]
      if (which === undefined) continue
      if (which === 'homeserverDetails' && !owner.target.joined) continue
      read(owner, which)
    }
  })

  // The bus is lossy, so a reconnect re-reads everything currently held. The
  // count handled is tracked here, not tested as `> 0`, so a panel opened long
  // after an old reconnect does not issue a second round of reads.
  let handledReconnects = live.reconnects.peek()
  const disposeReconnects = effect(() => {
    const reconnects = live.reconnects.value
    if (reconnects === handledReconnects) return
    handledReconnects = reconnects
    untracked(() => {
      const owner = session
      if (owner !== null) {
        for (const which of [
          'metadata',
          'info',
          'homeserverDetails',
        ] as const) {
          if (state.value[which].requested) read(owner, which)
        }
        if (state.value.hierarchy.requested) {
          reread(owner, {
            mode: 'reconnect',
            enrich: false,
            origin: 'reconnect',
          })
        }
      }
      if (previewSession !== null && preview.value.discovery.requested) {
        readPreview(previewSession)
      }
    })
  })

  function retry(): void {
    const owner = session
    if (owner === null) return
    for (const which of ['metadata', 'info', 'homeserverDetails'] as const) {
      if (which === 'homeserverDetails' && !owner.target.joined) continue
      if (state.value[which].error !== null) read(owner, which)
    }
    const failed = owner.hierarchy.failed
    if (state.value.hierarchy.error !== null && failed !== null) {
      if (failed === 'more') loadMore()
      else reread(owner, { ...failed, origin: 'retry' })
    }
  }

  function dispose(): void {
    show(null)
    previewSession?.controller.abort()
    previewSession = null
    unsubscribe()
    disposeReconnects()
    if (typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', onVisibilityChange)
    }
  }

  return {
    state,
    preview,
    show,
    loadMore,
    retry,
    showPreview,
    closePreview,
    retryPreview,
    dispose,
  }
}
