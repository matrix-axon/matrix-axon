import {
  computed,
  signal,
  type ReadonlySignal,
  type Signal,
} from '@preact/signals'
import type { components } from '../api/schema'
import { apiErrorMessage, type ApiClient } from '../api/client'
import type { RoomsStore } from './rooms'

export type InviteDto = components['schemas']['InviteDto']

export type InviteMutationResult = { ok: true } | { ok: false; message: string }

export type InviteBulkResult = {
  succeeded: number
  failed: { invite: InviteDto; message: string }[]
}

export interface InvitesStore {
  invites: ReadonlySignal<InviteDto[]>
  count: ReadonlySignal<number>
  loading: ReadonlySignal<boolean>
  error: Signal<string | null>
  refresh(): Promise<void>
  ensureLoaded(): Promise<void>
  resetSession(): void
  accept(invite: InviteDto): Promise<InviteMutationResult>
  reject(invite: InviteDto): Promise<InviteMutationResult>
  acceptAll(): Promise<InviteBulkResult>
  rejectAll(): Promise<InviteBulkResult>
  noteAdded(invite: InviteDto): void
  noteRemoved(accountId: string, roomId: string): void
}

export function inviteKey(
  invite: Pick<InviteDto, 'account_id' | 'room_id'>,
): string {
  return `${invite.account_id}/${invite.room_id}`
}

export function createInvitesStore(
  api: ApiClient,
  rooms: RoomsStore,
): InvitesStore {
  const invites = signal<InviteDto[]>([])
  const count = computed(() => invites.value.length)
  const loading = signal(true)
  const error = signal<string | null>(null)
  let settled = false
  let sessionGeneration = 0
  /** The GET in flight, if any. */
  let refreshing: Promise<void> | null = null
  /** At most one trailing GET, shared by callers that arrived while
   * `refreshing` was already running and so cannot use its older response. */
  let queued: Promise<void> | null = null
  /** Keys we accepted or rejected locally. A GET that still carries them
   * (watcher lag) must not put them back; a later GET that omits them
   * clears the suppression so a fresh re-invite can appear. Only *local*
   * mutations belong here — see `noteRemoved`. */
  const resolvedKeys = new Set<string>()
  /** Invites announced by a live frame since the in-flight GET was issued.
   * That response predates them, so assigning it wholesale would drop the
   * row; there is no periodic refresh, so it would stay gone until the next
   * reconnect. Reassigned (not mutated) per fetch so a late frame from an
   * abandoned fetch cannot leak into the next one. */
  let liveAdds = new Map<string, InviteDto>()

  function resetSession(): void {
    sessionGeneration += 1
    if (
      invites.value.length === 0 &&
      !settled &&
      error.value === null &&
      loading.value
    ) {
      return
    }
    invites.value = []
    resolvedKeys.clear()
    liveAdds = new Map()
    settled = false
    error.value = null
    loading.value = true
  }

  async function doRefresh(): Promise<void> {
    const generation = sessionGeneration
    loading.value = invites.value.length === 0
    const added = new Map<string, InviteDto>()
    liveAdds = added
    try {
      const { data, error: apiError } = await api.GET('/v1/invites')
      if (generation !== sessionGeneration) {
        return
      }
      if (apiError !== undefined || data === undefined) {
        error.value =
          apiError === undefined
            ? 'invite list failed'
            : apiErrorMessage(apiError)
        return
      }
      const serverKeys = new Set(data.data.map((row) => inviteKey(row)))
      for (const key of [...resolvedKeys]) {
        if (!serverKeys.has(key)) {
          resolvedKeys.delete(key)
        }
      }
      const next = data.data.filter((row) => !resolvedKeys.has(inviteKey(row)))
      // Frames that arrived after this GET was issued describe invites the
      // response cannot know about. Keep them rather than let a list fetched
      // moments earlier delete the row.
      // `resolvedKeys ⊆ serverKeys` after the cleanup above, so a key
      // missing from the GET cannot still be suppressed.
      for (const [key, invite] of added) {
        if (!serverKeys.has(key)) {
          next.unshift(invite)
        }
      }
      invites.value = next
      error.value = null
      settled = true
    } catch (cause) {
      if (generation !== sessionGeneration) {
        return
      }
      error.value = cause instanceof Error ? cause.message : String(cause)
    } finally {
      if (generation === sessionGeneration) {
        loading.value = false
      }
    }
  }

  function refresh(): Promise<void> {
    if (refreshing !== null) {
      // Do not hand back the in-flight promise: a caller that awaits
      // `refresh()` *after* a mutation would resolve against a response
      // fetched before it, and `runBulk` would set `settled` from
      // pre-mutation data. Chain one trailing fetch instead, shared by every
      // caller that lands here, so this stays a single extra request.
      queued ??= refreshing.then(() => {
        queued = null
        return refresh()
      })
      return queued
    }
    const started = doRefresh().finally(() => {
      if (refreshing === started) {
        refreshing = null
      }
    })
    refreshing = started
    return started
  }

  function ensureLoaded(): Promise<void> {
    return settled ? Promise.resolve() : refresh()
  }

  function removeLocal(accountId: string, roomId: string): void {
    liveAdds.delete(`${accountId}/${roomId}`)
    invites.value = invites.value.filter(
      (invite) => invite.account_id !== accountId || invite.room_id !== roomId,
    )
  }

  /** Drop a row we just accepted or rejected ourselves, and suppress it until
   * the server agrees it is gone. */
  function dropLocal(accountId: string, roomId: string): void {
    resolvedKeys.add(`${accountId}/${roomId}`)
    removeLocal(accountId, roomId)
  }

  async function mutate(
    invite: InviteDto,
    kind: 'accept' | 'reject',
  ): Promise<InviteMutationResult> {
    // Call the membership verbs directly. `rooms.leaveRoom` / `joinRoom`
    // refresh the joined-room list after every call and coalesce those
    // refreshes, which serialized two bulk rejects behind one GET /v1/rooms
    // and let a stale GET /v1/invites put a just-declined row back.
    try {
      const { error: apiError } =
        kind === 'accept'
          ? await api.POST('/v1/accounts/{account_id}/rooms/join', {
              params: { path: { account_id: invite.account_id } },
              body: { room_id_or_alias: invite.room_id },
            })
          : await api.POST('/v1/accounts/{account_id}/rooms/{room_id}/leave', {
              params: {
                path: {
                  account_id: invite.account_id,
                  room_id: invite.room_id,
                },
              },
            })
      if (apiError !== undefined) {
        return { ok: false, message: apiErrorMessage(apiError) }
      }
      dropLocal(invite.account_id, invite.room_id)
      return { ok: true }
    } catch (cause) {
      return {
        ok: false,
        message: cause instanceof Error ? cause.message : String(cause),
      }
    }
  }

  async function accept(invite: InviteDto): Promise<InviteMutationResult> {
    const result = await mutate(invite, 'accept')
    if (result.ok) {
      void rooms.refresh()
      void refresh()
    }
    return result
  }

  async function reject(invite: InviteDto): Promise<InviteMutationResult> {
    const result = await mutate(invite, 'reject')
    if (result.ok) {
      void refresh()
    }
    return result
  }

  async function runBulk(kind: 'accept' | 'reject'): Promise<InviteBulkResult> {
    // Never mutate from an unconfirmed list (ADR 0085 / issues 101 and 102).
    if (!settled) {
      await refresh()
    }
    if (!settled) {
      return { succeeded: 0, failed: [] }
    }
    const snapshot = invites.value.slice()
    const failed: InviteBulkResult['failed'] = []
    let succeeded = 0
    // One at a time: two concurrent leaves through the same SDK client
    // on a demo homeserver dropped the second invite. Accept/reject all
    // can wait; losing a row cannot.
    for (const invite of snapshot) {
      const result = await mutate(invite, kind)
      if (result.ok) {
        succeeded += 1
      } else {
        failed.push({ invite, message: result.message })
      }
    }
    if (kind === 'accept' && succeeded > 0) {
      void rooms.refresh()
    }
    await refresh()
    return { succeeded, failed }
  }

  function noteAdded(invite: InviteDto): void {
    const key = inviteKey(invite)
    // The server asserting the invite exists right now outranks a stale local
    // suppression, so clear the key rather than drop the frame. Dropping it
    // is what made a re-invite permanently invisible: every later GET then
    // carries the key, so the "a GET that omits it clears the suppression"
    // rule never fires. The cost is that an `invite.added` still in flight
    // when the user resolves the row can briefly re-show it, which the
    // follow-up refresh corrects.
    resolvedKeys.delete(key)
    const next = invites.value.filter((row) => inviteKey(row) !== key)
    next.unshift(invite)
    invites.value = next
    liveAdds.set(key, invite)
    // Deliberately not `settled = true`: one frame is content, not a
    // complete list, and `settled` is what lets `runBulk` skip its
    // confirming refresh and act on everything it can see.
    loading.value = false
  }

  function noteRemoved(accountId: string, roomId: string): void {
    // Server-driven, so there is nothing to suppress — the row is already
    // gone upstream and no GET can put it back. Adding the key to
    // `resolvedKeys` here would outlive the removal and hide a later
    // re-invite for the rest of the session.
    removeLocal(accountId, roomId)
  }

  return {
    invites,
    count,
    loading,
    error,
    refresh,
    ensureLoaded,
    resetSession,
    accept,
    reject,
    acceptAll: () => runBulk('accept'),
    rejectAll: () => runBulk('reject'),
    noteAdded,
    noteRemoved,
  }
}
