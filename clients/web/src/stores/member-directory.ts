import { signal } from '@preact/signals'
import type { MemberDto } from './room-list'

/**
 * Account-wide names and avatars, learned from every member list this client
 * reads, so a room whose own list has not arrived yet can still label senders.
 *
 * Without it, every room open painted raw Matrix ids until that room's
 * `/members` answered. On a slow link that is seconds, and most senders in
 * one room are the same people as in the last: the client had their names
 * already, under a different room.
 *
 * A fallback, not the truth. Matrix display names are per room, so a user can
 * carry a different name here than in the room being read; the room's own
 * member list always wins once it lands (`MembersStore.displayName`). In
 * memory only — the same footing as the warm timeline stores — and wiped on
 * sign-out.
 */
export interface MemberDirectory {
  /**
   * The best-known profile for a user, from any room on this account. Reads a
   * signal, so a render that falls back to it updates when a list lands.
   */
  lookup(accountId: string, userId: string): MemberDto | undefined
  /** Learn from one room's member list. */
  record(accountId: string, members: readonly MemberDto[]): void
  /** Forget everything. Idempotent: writes nothing when already empty. */
  clear(): void
  /** Known users, for tests and diagnostics. */
  readonly size: number
}

/** Current members outrank departed ones: a name from a room someone left is older. */
function current(member: MemberDto): boolean {
  return member.membership === 'join' || member.membership === 'invite'
}

function hasProfile(member: MemberDto): boolean {
  return (
    (member.display_name?.trim() ?? '') !== '' ||
    (member.avatar_url ?? '') !== ''
  )
}

export function createMemberDirectory(): MemberDirectory {
  const entries = signal<ReadonlyMap<string, MemberDto>>(new Map())
  const key = (accountId: string, userId: string) => `${accountId}\0${userId}`

  return {
    lookup(accountId, userId) {
      return entries.value.get(key(accountId, userId))
    },

    record(accountId, members) {
      let next: Map<string, MemberDto> | null = null
      for (const member of members) {
        // Nothing to lend: the id fallback is what the room would show anyway.
        if (!hasProfile(member)) {
          continue
        }
        const k = key(accountId, member.user_id)
        const known = (next ?? entries.peek()).get(k)
        if (known !== undefined) {
          if (current(known) && !current(member)) {
            continue
          }
          if (
            known.display_name === member.display_name &&
            known.avatar_url === member.avatar_url
          ) {
            continue
          }
        }
        next ??= new Map(entries.peek())
        next.set(k, member)
      }
      // One write per list, and none when nothing changed: a room switch
      // re-reads a list it has seen, and every fallback render subscribes.
      if (next !== null) {
        entries.value = next
      }
    },

    clear() {
      if (entries.peek().size > 0) {
        entries.value = new Map()
      }
    },

    get size() {
      return entries.peek().size
    },
  }
}
