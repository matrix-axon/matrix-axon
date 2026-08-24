import type { components } from '../api/schema'
import type { RoomFilter, RoomSort } from './settings'

export type RoomDto = components['schemas']['RoomDto']
export type MemberDto = components['schemas']['MemberDto']

/**
 * Pure room-list semantics (ADR 0046, M-W4), ported from the TUI
 * (`clients/tui/src/app/rooms.rs`) so the two clients slice the list the same
 * way: DM heuristic, member-derived titles, pin-aware sorting (ADR 0038), and
 * the filter modes (ADR 0042).
 */

/**
 * Room identity across accounts: `(account_id, room_id)` — a room joined by
 * two accounts appears twice in the list. The account id is a UUID (no `/`),
 * so `/` is an unambiguous separator; room ids contain `:` so that is not.
 */
export function roomKey(room: Pick<RoomDto, 'account_id' | 'room_id'>): string {
  return `${room.account_id}/${room.room_id}`
}

function blank(value: string | null | undefined): boolean {
  return value === null || value === undefined || value.trim() === ''
}

/**
 * Whether a room is *likely* a DM (ADR 0042). Interim heuristic: no
 * `m.room.name` and no canonical alias. Imperfect (a named two-person room
 * reads as a group, an unnamed small group as a DM); slated to be replaced by
 * the server-derived `is_direct` from ADR 0055 — swap the body here when that
 * lands, exactly as the TUI plans to.
 */
export function isLikelyDm(room: RoomDto): boolean {
  return blank(room.name) && blank(room.canonical_alias)
}

/**
 * A user id rendered without a known display name: `@localpart`, falling back
 * to the full id when it doesn't parse. The last resort of every name
 * resolution in the app, so it lives in one place.
 */
export function userIdDisplay(userId: string): string {
  const local = /^@([^:]+):/.exec(userId)?.[1]
  return local !== undefined ? `@${local}` : userId
}

/** A member's display name, falling back to `@localpart`, then the user id. */
export function memberDisplay(member: MemberDto): string {
  const name = member.display_name?.trim()
  if (name !== undefined && name !== '') {
    return name
  }
  return userIdDisplay(member.user_id)
}

/**
 * Derive a display title for an unnamed room (e.g. a DM) from its member
 * list, excluding the account's own user. Lists up to three names and appends
 * `, +N` for the rest; `null` when there is no other member (a note-to-self
 * room), so the caller keeps its fallback.
 */
export function dmTitleFromMembers(
  selfUserId: string | null,
  members: MemberDto[],
): string | null {
  const others = members
    .filter((member) => member.user_id !== selfUserId)
    // Stable ordering so the title doesn't reshuffle between fetches.
    .sort((a, b) =>
      a.user_id < b.user_id ? -1 : a.user_id > b.user_id ? 1 : 0,
    )
  if (others.length === 0) {
    return null
  }
  let title = others.slice(0, 3).map(memberDisplay).join(', ')
  if (others.length > 3) {
    title += `, +${others.length - 3}`
  }
  return title
}

/**
 * The other person's avatar in a 1:1 DM, or `null` when there isn't exactly
 * one other joined/invited member with an `mxc://` avatar. Unnamed rooms with
 * several people keep the letter fallback unless the room itself has an
 * avatar — this is the DM case only.
 */
export function dmPeerAvatarFromMembers(
  selfUserId: string | null,
  members: readonly MemberDto[],
): string | null {
  const others = members.filter(
    (member) =>
      member.user_id !== selfUserId &&
      (member.membership === 'join' || member.membership === 'invite'),
  )
  if (others.length !== 1) {
    return null
  }
  const avatar = others[0].avatar_url
  return blank(avatar) ? null : avatar!
}

/**
 * Avatar shown on a room-list row: the room's own `avatar_url` when set,
 * else a cached DM peer avatar, else none (the colored letter).
 */
export function roomListAvatarUrl(
  room: RoomDto,
  dmAvatars: ReadonlyMap<string, string>,
): string | null {
  if (!blank(room.avatar_url)) {
    return room.avatar_url!
  }
  if (!isLikelyDm(room)) {
    return null
  }
  return dmAvatars.get(roomKey(room)) ?? null
}

/**
 * The rendered room-list title: name, else canonical alias, else the cached
 * member-derived title for unnamed rooms, else the room id. Matches the TUI's
 * `room_list_title_from_cache`.
 */
export function roomTitle(
  room: RoomDto,
  titleCache: ReadonlyMap<string, string>,
): string {
  if (!blank(room.name)) {
    return room.name!
  }
  if (!blank(room.canonical_alias)) {
    return room.canonical_alias!
  }
  return titleCache.get(roomKey(room)) ?? room.room_id
}

/** `@adam:bostoncoop.net` → `@adam`. A user id with no server part is itself. */
export function localpart(userId: string): string {
  const colon = userId.indexOf(':')
  return colon === -1 ? userId : userId.slice(0, colon)
}

/**
 * Per-account labels for the room rows, keyed by account id: the localpart
 * alone when that is unambiguous, else the full user id. Two accounts on
 * different homeservers can share a localpart (`@adam:a.net`, `@adam:b.net`),
 * and a row that cannot tell them apart is worse than a long one. The account
 * dropdown always shows full ids, so the mapping is recoverable either way.
 */
export function accountLabels(
  accounts: readonly (readonly [string, string])[],
): Map<string, string> {
  const shortened = accounts.map(([, userId]) => localpart(userId))
  const ambiguous = new Set(shortened).size !== shortened.length
  return new Map(
    accounts.map(([id, userId], index) => [
      id,
      ambiguous ? userId : shortened[index],
    ]),
  )
}

/**
 * Order rooms with pinned rooms first — by their position in `pinned`, most
 * recently pinned first (ADR 0038) — then the unpinned tail by the active
 * sort (ADR 0042). The pinned section keeps pin order in every mode, since
 * distinct pin ranks never reach the tiebreak. Alphabetical modes compare the
 * lowercased rendered title, so unnamed DMs sort by their member-derived
 * names once known instead of by opaque room ids.
 */
export function sortRooms(
  rooms: RoomDto[],
  pinned: readonly string[],
  sort: RoomSort,
  title: (room: RoomDto) => string,
): RoomDto[] {
  // Decorate–sort–undecorate (WCR-13): the comparator used to recompute
  // `roomKey` + a linear `pinned.indexOf` + `title().toLowerCase()` per
  // *comparison* — O(n log n) string builds and pin scans on a
  // thousand-room list, re-run on every render of the list. Each room's
  // rank and sort key are computed exactly once here.
  const pinRank = new Map(pinned.map((key, index) => [key, index]))
  const alphabetical = sort === 'az' || sort === 'za'
  const decorated = rooms.map((room) => ({
    room,
    rank: pinRank.get(roomKey(room)) ?? Number.MAX_SAFE_INTEGER,
    title: alphabetical ? title(room).toLowerCase() : '',
  }))
  type Decorated = (typeof decorated)[number]
  const tiebreak = (a: Decorated, b: Decorated): number => {
    switch (sort) {
      case 'recent':
        return b.room.last_activity_ts - a.room.last_activity_ts
      case 'oldest':
        return a.room.last_activity_ts - b.room.last_activity_ts
      case 'az':
        return a.title.localeCompare(b.title)
      case 'za':
        return b.title.localeCompare(a.title)
    }
  }
  return decorated
    .sort((a, b) => a.rank - b.rank || tiebreak(a, b))
    .map((entry) => entry.room)
}

/** The active filter: a persisted category, or a session-only name query. */
export type ActiveFilter =
  { kind: RoomFilter } | { kind: 'name'; query: string }

/**
 * Apply the active filter (ADR 0042). `name` matches a case-insensitive
 * substring of the room name, canonical alias, topic, room id, or the same
 * rendered title the list shows, so member-derived DM titles match too.
 */
export function filterRooms(
  rooms: RoomDto[],
  filter: ActiveFilter,
  context: {
    hasUnread: (key: string, room: RoomDto) => boolean
    isPinned: (key: string) => boolean
    title: (room: RoomDto) => string
  },
): RoomDto[] {
  switch (filter.kind) {
    case 'all':
      return rooms
    case 'unread':
      return rooms.filter((room) => context.hasUnread(roomKey(room), room))
    case 'dms':
      return rooms.filter(isLikelyDm)
    case 'groups':
      return rooms.filter((room) => !isLikelyDm(room))
    case 'favorites':
      return rooms.filter((room) => context.isPinned(roomKey(room)))
    case 'name': {
      const query = filter.query.trim().toLowerCase()
      if (query === '') {
        return rooms
      }
      return rooms.filter((room) =>
        [
          room.name,
          room.canonical_alias,
          room.topic,
          room.room_id,
          context.title(room),
        ].some((field) => field?.toLowerCase().includes(query) ?? false),
      )
    }
  }
}
