import {
  computed,
  signal,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import { apiErrorCode, apiErrorMessage, type ApiClient } from '../api/client'
import { perfMark } from '../perf'
import { memberDisplay, type MemberDto } from './room-list'

export type MemberMutationResult =
  { ok: true } | { ok: false; message: string; code: string | null }

export interface MembersStore {
  members: ReadonlySignal<ReadonlyMap<string, MemberDto>>
  loading: ReadonlySignal<boolean>
  error: Signal<string | null>

  /** Resolved display name for a sender id, falling back per `memberDisplay`. */
  displayName(userId: string): string

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
 * back to `@localpart`, then the full user id) instead of the raw Matrix id —
 * the same resolution `dmTitleFromMembers` already does for room titles.
 */
export function createMembersStore(
  api: ApiClient,
  accountId: string,
  roomId: string,
): MembersStore {
  const members = signal<ReadonlyMap<string, MemberDto>>(new Map())
  const loading = signal(true)
  const error = signal<string | null>(null)

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
      const member = members.value.get(userId)
      return member !== undefined ? memberDisplay(member) : userId
    },

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
