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
  /**
   * Client time the latest response arrived. Server timestamps in it
   * (`expires_at`, `next_attempt_at`) are judged against this, so a label
   * describes the answer as of its read, and changes only with the next read —
   * which every change the server stores prompts with a frame.
   */
  receivedAt: number | null
}

export interface RoomDetailsState {
  /** `roomDetailsKey` of the room the slots describe, or `null` for none. */
  key: string | null
  metadata: RoomDetailsSlot<RoomMetadataDto>
  homeserverDetails: RoomDetailsSlot<RoomHomeserverDetailsDto>
  info: RoomDetailsSlot<RoomInfoDto>
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
}

/** A composite in-memory key (`'\0'`-joined; `clients/web/AGENTS.md`). */
export function roomDetailsKey(accountId: string, roomId: string): string {
  return `${accountId}\0${roomId}`
}

function emptySlot<T>(): RoomDetailsSlot<T> {
  return { value: null, error: null, requested: false, receivedAt: null }
}

export function emptyRoomDetails(key: string | null): RoomDetailsState {
  return {
    key,
    metadata: emptySlot(),
    homeserverDetails: emptySlot(),
    info: emptySlot(),
  }
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
        const slot = state.value[which]
        state.value = {
          ...state.value,
          [which]: result.ok
            ? {
                value: result.value,
                error: null,
                requested: true,
                receivedAt: Date.now(),
              }
            : { ...slot, error: result.message },
        }
        if (owner.dirty.delete(which)) read(owner, which)
      },
    )
  }

  function show(target: RoomDetailsTarget | null): void {
    const key =
      target === null ? null : roomDetailsKey(target.accountId, target.roomId)
    if (session !== null && key === session.key && target !== null) {
      const becameJoined = target.joined && !session.target.joined
      session.target = target
      if (becameJoined) read(session, 'homeserverDetails')
      return
    }
    session?.controller.abort()
    session = null
    state.value = emptyRoomDetails(key)
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

  live.subscribe((frame) => {
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
  effect(() => {
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

  return { state, show }
}
