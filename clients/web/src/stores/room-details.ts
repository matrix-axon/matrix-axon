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
 * `/pinned`, `/space/parents`, `/upgrade` and `/power_levels` stay in the
 * panel's own loader; they are not part of issue 618.
 */
import { effect, signal, untracked, type ReadonlySignal } from '@preact/signals'
import { apiErrorMessage, type ApiClient } from '../api/client'
import { roomDetailsChange, type RoomDetailSource } from '../api/frames'
import type { components } from '../api/schema'
import type { LiveConnection } from './live-connection'

type RoomMetadataDto = components['schemas']['RoomMetadataDto']
type RoomHomeserverDetailsDto =
  components['schemas']['RoomHomeserverDetailsDto']
type RoomInfoDto = components['schemas']['RoomInfoDto']

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

export interface RoomDetailsState {
  /** `roomDetailsKey` of the room the slots describe, or `null` for none. */
  key: string | null
  metadata: RoomDetailsSlot<RoomMetadataDto>
  homeserverDetails: RoomDetailsSlot<RoomHomeserverDetailsDto>
  info: RoomDetailsSlot<RoomInfoDto>
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
}

export interface RoomDetailsStore {
  state: ReadonlySignal<RoomDetailsState>
  /**
   * Show `target`'s details, or none. A new room aborts the previous room's
   * reads and starts its own; the same room again only adds a read that has
   * become due (`/homeserver_details` once the room is known to be joined).
   * Showing `null` then the same room again is a reopen, and re-reads.
   */
  show(target: RoomDetailsTarget | null): void
  /**
   * Repeat every read whose latest attempt failed, for the room shown. The
   * panel's Retry: a failure otherwise waits for a frame, a reconnect or a
   * reopen.
   */
  retry(): void
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

export function emptyRoomDetails(
  key: string | null,
  now = 0,
): RoomDetailsState {
  return {
    key,
    metadata: emptySlot(),
    homeserverDetails: emptySlot(),
    info: emptySlot(),
    now,
  }
}

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

/** One shown room: its identity, its reads' abort handle, and their bookkeeping. */
interface Session {
  key: string
  target: RoomDetailsTarget
  controller: AbortController
  inFlight: Set<RoomDetailsRead>
  /** A frame arrived mid-read: exactly one more read follows the current one. */
  dirty: Set<RoomDetailsRead>
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

  function show(target: RoomDetailsTarget | null): void {
    const key =
      target === null ? null : roomDetailsKey(target.accountId, target.roomId)
    if (session !== null && key === session.key && target !== null) {
      const becameJoined = target.joined && !session.target.joined
      const left = !target.joined && session.target.joined
      session.target = target
      if (becameJoined) read(session, 'homeserverDetails')
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
    session?.controller.abort()
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
    }
    session = owner
    read(owner, 'metadata')
    read(owner, 'info')
    if (target.joined) read(owner, 'homeserverDetails')
  }

  const unsubscribe = live.subscribe((frame) => {
    const change = roomDetailsChange(frame)
    const owner = session
    if (
      change === null ||
      owner === null ||
      frame.accountId !== owner.target.accountId ||
      change.roomId !== owner.target.roomId
    ) {
      return
    }
    for (const source of change.sources) {
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
      if (owner === null) return
      for (const which of ['metadata', 'info', 'homeserverDetails'] as const) {
        if (state.value[which].requested) read(owner, which)
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
  }

  function dispose(): void {
    show(null)
    unsubscribe()
    disposeReconnects()
    if (typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', onVisibilityChange)
    }
  }

  return { state, show, retry, dispose }
}
