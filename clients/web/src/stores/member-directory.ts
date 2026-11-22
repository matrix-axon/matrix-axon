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
  /**
   * The account's write epoch. Take it *before* asking for a member list and
   * hand it to `record`: a list that comes back after `clear` or
   * `forgetAccount` is then dropped instead of putting names back.
   */
  epoch(accountId: string): number
  /**
   * Learn from one room's member list, unless the account's epoch has moved
   * since `epoch` was taken.
   */
  record(accountId: string, members: readonly MemberDto[], epoch: number): void
  /** Forget one account's users: logged out or deleted, session still on. */
  forgetAccount(accountId: string): void
  /** Forget everything. Idempotent: writes nothing when already empty. */
  clear(): void
  /** Known users, for tests and diagnostics. */
  readonly size: number
}

/** Current members outrank departed ones: a name from a room someone left is older. */
function current(member: MemberDto): boolean {
  return member.membership === 'join' || member.membership === 'invite'
}
/** A field worth lending: a name that is not blank, an avatar that is set. */
function filled(value: string | null | undefined): value is string {
  return (value?.trim() ?? '') !== ''
}

function hasProfile(member: MemberDto): boolean {
  return filled(member.display_name) || filled(member.avatar_url)
}

/**
 * `member` laid over what is already known. A blank field keeps the known
 * value: a room whose member event carries a name and no avatar is the usual
 * case, and must not strip the avatar another room lent, nor a nameless row
 * strip a name. The room's own list still wins on its own page.
 */
function merged(known: MemberDto | undefined, member: MemberDto): MemberDto {
  if (known === undefined) {
    return member
  }
  return {
    ...member,
    display_name: filled(member.display_name)
      ? member.display_name
      : known.display_name,
    avatar_url: filled(member.avatar_url)
      ? member.avatar_url
      : known.avatar_url,
  }
}

export function createMemberDirectory(): MemberDirectory {
  const entries = signal<ReadonlyMap<string, MemberDto>>(new Map())
  const key = (accountId: string, userId: string) => `${accountId}\0${userId}`
  // One counter for both kinds of wipe, so an account's epoch only rises:
  // `clear` lifts every account to `wipedAll`, `forgetAccount` one past it.
  let counter = 0
  let wipedAll = 0
  const forgotten = new Map<string, number>()
  const epoch = (accountId: string) =>
    Math.max(wipedAll, forgotten.get(accountId) ?? 0)

  return {
    lookup(accountId, userId) {
      return entries.value.get(key(accountId, userId))
    },

    epoch,

    record(accountId, members, issued) {
      // Asked for before a sign-out or an account's removal: the names it
      // carries belong to a reader who is gone.
      if (issued !== epoch(accountId)) {
        return
      }
      let next: Map<string, MemberDto> | null = null
      for (const member of members) {
        // Nothing to lend: the id fallback is what the room would show anyway.
        if (!hasProfile(member)) {
          continue
        }
        const k = key(accountId, member.user_id)
        const known = (next ?? entries.peek()).get(k)
        if (known !== undefined && current(known) && !current(member)) {
          continue
        }
        const entry = merged(known, member)
        if (
          known !== undefined &&
          known.display_name === entry.display_name &&
          known.avatar_url === entry.avatar_url &&
          current(known) === current(entry)
        ) {
          continue
        }
        next ??= new Map(entries.peek())
        next.set(k, entry)
      }
      // One write per list, and none when nothing changed: a room switch
      // re-reads a list it has seen, and every fallback render subscribes.
      if (next !== null) {
        entries.value = next
      }
    },

    forgetAccount(accountId) {
      forgotten.set(accountId, ++counter)
      const prefix = `${accountId}\0`
      const kept = [...entries.peek()].filter(([k]) => !k.startsWith(prefix))
      if (kept.length !== entries.peek().size) {
        entries.value = new Map(kept)
      }
    },

    clear() {
      // Moves the epoch even when empty: a list may be in flight regardless.
      wipedAll = ++counter
      if (entries.peek().size > 0) {
        entries.value = new Map()
      }
    },

    get size() {
      return entries.peek().size
    },
  }
}
