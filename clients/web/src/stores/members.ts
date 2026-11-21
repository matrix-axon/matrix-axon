import {
  computed,
  signal,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import { apiErrorCode, apiErrorMessage, type ApiClient } from '../api/client'
import { perfMark } from '../perf'
import type { MemberDirectory } from './member-directory'
import { memberDisplay, type MemberDto } from './room-list'

export type MemberMutationResult =
  { ok: true } | { ok: false; message: string; code: string | null }

export interface MembersStore {
  members: ReadonlySignal<ReadonlyMap<string, MemberDto>>
  loading: ReadonlySignal<boolean>
  error: Signal<string | null>

  /**
   * Resolved display name for a sender id, falling back per `memberDisplay`.
   * Until this room's list has the user, the account-wide directory lends the
   * name another room gave them; the raw id comes back only when neither does.
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

  function profile(userId: string): MemberDto | undefined {
    // The room's own entry first, and only then the directory: reading the
    // directory subscribes the render to every list that lands anywhere.
    return members.value.get(userId) ?? directory?.lookup(accountId, userId)
  }

  async function refresh() {
    // The member list is unpaginated, so on a weak link it is one of the two
    // large bodies a room open puts on the wire alongside the timeline page
    // that actually gates first paint. `members` is the size that matters.
    perfMark('members:refresh:start', { roomId })
    try {
      const { data, error: apiError } = await api.GET(
        '/v1/accounts/{account_id}/rooms/{room_id}/members',
        { params: { path: { account_id: accountId, room_id: roomId } } },
      )
      if (apiError !== undefined) {
        error.value = apiErrorMessage(apiError)
        return
      }
      error.value = null
      members.value = new Map(
        data.data.map((member) => [member.user_id, member]),
      )
      directory?.record(accountId, data.data)
    } catch (cause) {
      error.value = cause instanceof Error ? cause.message : String(cause)
    } finally {
      loading.value = false
      perfMark('members:refresh:end', {
        roomId,
        members: members.value.size,
        ok: error.value === null,
      })
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

/** Rooms whose member list stays warm across switches; the timeline cache's cap. */
export const MEMBERS_STORE_LIMIT = 30

/**
 * Warm per-room member stores, keyed by account and room — the members half
 * of ADR 0085 phase 1. `RoomPage` used to build one per mount, so returning
 * to a room showed raw ids until its `/members` answered again. A warm store
 * paints the names it had and refreshes behind them (the page still calls
 * `refresh()` on entry).
 *
 * Bounded like the timeline cache. A member list is unpaginated, so one huge
 * room is the worst case (a few thousand entries, well under a megabyte); the
 * median room holds a handful.
 */
export interface MembersStoreCache {
  /** The warm store for a room, or a fresh one. Marks it most recently used. */
  acquire(accountId: string, roomId: string): MembersStore
  /** Drop every store. Sign-out only. */
  clear(): void
  /** Warm room count, for tests and diagnostics. */
  readonly size: number
}

export function createMembersStoreCache(
  api: ApiClient,
  directory: MemberDirectory | null = null,
  limit: number = MEMBERS_STORE_LIMIT,
): MembersStoreCache {
  // Insertion order is LRU order, as in `createTimelineStoreCache`.
  const stores = new Map<string, MembersStore>()
  return {
    acquire(accountId, roomId) {
      const key = `${accountId}\0${roomId}`
      const warm = stores.get(key)
      if (warm !== undefined) {
        stores.delete(key)
        stores.set(key, warm)
        return warm
      }
      const store = createMembersStore(api, accountId, roomId, directory)
      stores.set(key, store)
      for (const oldest of stores.keys()) {
        if (stores.size <= limit) {
          break
        }
        stores.delete(oldest)
      }
      return store
    },

    clear() {
      stores.clear()
    },

    get size() {
      return stores.size
    },
  }
}
