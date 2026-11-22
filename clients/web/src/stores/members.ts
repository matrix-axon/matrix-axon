import {
  computed,
  signal,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import { apiErrorCode, apiErrorMessage, type ApiClient } from '../api/client'
import { perfMark } from '../perf'
import type { MemberDirectory } from './member-directory'
import { createRoomStoreCache, type RoomStoreCache } from './room-store-cache'
import { memberDisplay, type MemberDto } from './room-list'

export type MemberMutationResult =
  { ok: true } | { ok: false; message: string; code: string | null }

export interface MembersStore {
  members: ReadonlySignal<ReadonlyMap<string, MemberDto>>
  loading: ReadonlySignal<boolean>
  error: Signal<string | null>

  /**
   * Resolved display name for a sender id, falling back per `memberDisplay`.
   * Until this room's first list lands, the account-wide directory lends the
   * name another room gave them. After it, the room's list is the answer: a
   * sender missing from it (a bridge ghost, say) shows the raw id, not a name
   * some other room gave them.
   */
  displayName(userId: string): string
  /**
   * The profile to *draw* a user with — avatar and name — on the same terms
   * as `displayName`. Not membership: `members` stays this room's roster, and
   * a directory entry's `membership` describes some other room.
   */
  profile(userId: string): MemberDto | undefined

  /** Fetch the room's current member list. */
  refresh(): Promise<void>

  /** Invite one or more Matrix users to this room, then refresh the roster. */
  inviteUsers(userIds: readonly string[]): Promise<MemberMutationResult>

  /** Cancel a pending invite by moving that invited member back to leave. */
  cancelInvite(userId: string): Promise<MemberMutationResult>
}

/**
 * One room's members (ADR 0046), fetched once per room and used to resolve
 * senders in the timeline and thread panel to their displayname (falling
 * back to `@localpart`, then the full user id) instead of the raw Matrix id,
 * through `memberDisplay`.
 */
export function createMembersStore(
  api: ApiClient,
  accountId: string,
  roomId: string,
  directory: MemberDirectory | null = null,
): MembersStore {
  const members = signal<ReadonlyMap<string, MemberDto>>(new Map())
  const loading = signal(true)
  const error = signal<string | null>(null)
  /** True once a list for this room has landed; failed reads leave it false. */
  const listed = signal(false)

  function profile(userId: string): MemberDto | undefined {
    // The room's own entry first, and only then the directory: reading the
    // directory subscribes the render to every list that lands anywhere.
    // Matrix names are per room, so the directory only bridges the wait.
    return (
      members.value.get(userId) ??
      (listed.value ? undefined : directory?.lookup(accountId, userId))
    )
  }

  /**
   * Bumped by each `refresh`. A warm store outlives the visit that started a
   * refresh, so an older list can come back after a newer one; only the
   * latest may write the roster, the error or `loading`.
   */
  let refreshes = 0

  async function refresh() {
    // The member list is unpaginated, so on a weak link it is one of the two
    // large bodies a room open puts on the wire alongside the timeline page
    // that actually gates first paint. `members` is the size that matters.
    perfMark('members:refresh:start', { roomId })
    const issued = ++refreshes
    const epoch = directory?.epoch(accountId) ?? 0
    try {
      const { data, error: apiError } = await api.GET(
        '/v1/accounts/{account_id}/rooms/{room_id}/members',
        { params: { path: { account_id: accountId, room_id: roomId } } },
      )
      // Overtaken by a newer refresh: this list is older than the one that
      // will land, here and in the directory alike.
      if (issued !== refreshes) {
        return
      }
      if (apiError !== undefined) {
        error.value = apiErrorMessage(apiError)
        return
      }
      error.value = null
      members.value = new Map(
        data.data.map((member) => [member.user_id, member]),
      )
      listed.value = true
      // Dropped by the directory itself if the reader signed out, or this
      // account was removed, while the list was in flight.
      directory?.record(accountId, data.data, epoch)
    } catch (cause) {
      if (issued === refreshes) {
        error.value = cause instanceof Error ? cause.message : String(cause)
      }
    } finally {
      // Only the newest refresh ends the read. The room-open summary takes
      // the first end mark after its start, and an overtaken request is often
      // the previous visit's: its end would stamp this open with that
      // request's time. The newest always lands, so no start goes unanswered.
      if (issued === refreshes) {
        loading.value = false
        perfMark('members:refresh:end', {
          roomId,
          members: members.value.size,
          ok: error.value === null,
        })
      }
    }
  }

  return {
    members: computed(() => members.value),
    loading: computed(() => loading.value),
    error,

    displayName(userId: string): string {
      const member = profile(userId)
      return member !== undefined ? memberDisplay(member) : userId
    },

    profile,

    refresh,

    async inviteUsers(userIds) {
      let invitedAny = false
      try {
        for (const userId of userIds) {
          const { error: apiError } = await api.POST(
            '/v1/accounts/{account_id}/rooms/{room_id}/invite',
            {
              params: { path: { account_id: accountId, room_id: roomId } },
              body: { user_id: userId },
            },
          )
          if (apiError !== undefined) {
            const message = apiErrorMessage(apiError)
            if (invitedAny) {
              await refresh()
            }
            error.value = message
            return { ok: false, message, code: apiErrorCode(apiError) }
          }
          invitedAny = true
        }
        await refresh()
        return { ok: true }
      } catch (cause) {
        const message = cause instanceof Error ? cause.message : String(cause)
        if (invitedAny) {
          await refresh()
        }
        error.value = message
        return { ok: false, message, code: null }
      }
    },

    async cancelInvite(userId) {
      try {
        const { error: apiError } = await api.POST(
          '/v1/accounts/{account_id}/rooms/{room_id}/kick',
          {
            params: { path: { account_id: accountId, room_id: roomId } },
            body: { user_id: userId },
          },
        )
        if (apiError !== undefined) {
          const message = apiErrorMessage(apiError)
          error.value = message
          return { ok: false, message, code: apiErrorCode(apiError) }
        }
        await refresh()
        return { ok: true }
      } catch (cause) {
        const message = cause instanceof Error ? cause.message : String(cause)
        error.value = message
        return { ok: false, message, code: null }
      }
    },
  }
}

/**
 * Warm per-room member stores (`RoomStoreCache`): a return visit paints the
 * names it had rather than raw ids until `/members` answers again. A member
 * list is unpaginated, so one huge room is the worst case (a few thousand
 * entries, well under a megabyte); the median room holds a handful.
 */
export type MembersStoreCache = RoomStoreCache<MembersStore>

export function createMembersStoreCache(
  api: ApiClient,
  directory: MemberDirectory | null = null,
  limit?: number,
): MembersStoreCache {
  return createRoomStoreCache(
    (accountId, roomId) =>
      createMembersStore(api, accountId, roomId, directory),
    { limit },
  )
}
