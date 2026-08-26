import { describe, expect, it } from 'vitest'
import {
  accountLabels,
  dmPeerAvatarFromMembers,
  dmTitleFromMembers,
  filterRooms,
  isLikelyDm,
  localpart,
  roomKey,
  roomListAvatarUrl,
  roomTitle,
  sortRooms,
  type MemberDto,
  type RoomDto,
} from './room-list'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'

function room(overrides: Partial<RoomDto> & { room_id: string }): RoomDto {
  return {
    account_id: ACCOUNT,
    account_user_id: '@me:example.org',
    last_activity_ts: 0,
    highlight_count: 0,
    notification_count: 0,
    ...overrides,
  }
}

function member(
  user_id: string,
  display_name?: string,
  avatar_url?: string | null,
): MemberDto {
  return {
    user_id,
    membership: 'join',
    display_name: display_name ?? null,
    avatar_url: avatar_url ?? null,
  }
}

describe('roomKey', () => {
  it('separates account and room unambiguously', () => {
    expect(roomKey(room({ room_id: '!a:hs.example' }))).toBe(
      `${ACCOUNT}/!a:hs.example`,
    )
  })
})

describe('isLikelyDm (ADR 0042 interim heuristic)', () => {
  it('is true only when both name and alias are blank', () => {
    expect(isLikelyDm(room({ room_id: '!r:hs' }))).toBe(true)
    expect(isLikelyDm(room({ room_id: '!r:hs', name: '  ' }))).toBe(true)
    expect(isLikelyDm(room({ room_id: '!r:hs', name: 'Ops' }))).toBe(false)
    expect(
      isLikelyDm(room({ room_id: '!r:hs', canonical_alias: '#ops:hs' })),
    ).toBe(false)
  })
})

describe('dmTitleFromMembers (TUI parity)', () => {
  it('excludes self, prefers display names, falls back to @localpart', () => {
    const title = dmTitleFromMembers('@me:example.org', [
      member('@me:example.org', 'Me'),
      member('@bob:example.org', 'Bob'),
      member('@carol:example.org'),
    ])
    expect(title).toBe('Bob, @carol')
  })

  it('sorts by user id for stability and caps at three plus +N', () => {
    const title = dmTitleFromMembers('@me:x', [
      member('@d:x', 'Dan'),
      member('@b:x', 'Beth'),
      member('@a:x', 'Ann'),
      member('@c:x', 'Cid'),
      member('@e:x', 'Eve'),
    ])
    expect(title).toBe('Ann, Beth, Cid, +2')
  })

  it('returns null for a note-to-self room', () => {
    expect(dmTitleFromMembers('@me:x', [member('@me:x')])).toBeNull()
  })
})

describe('dmPeerAvatarFromMembers', () => {
  it('returns the other member’s avatar in a 1:1 DM', () => {
    expect(
      dmPeerAvatarFromMembers('@me:hs', [
        member('@me:hs', 'Me'),
        member('@bob:hs', 'Bob', 'mxc://hs/bob'),
      ]),
    ).toBe('mxc://hs/bob')
  })

  it('returns null when the peer has no avatar, or the roster is not 1:1', () => {
    expect(
      dmPeerAvatarFromMembers('@me:hs', [
        member('@me:hs'),
        member('@bob:hs', 'Bob'),
      ]),
    ).toBeNull()
    expect(
      dmPeerAvatarFromMembers('@me:hs', [
        member('@me:hs'),
        member('@bob:hs', 'Bob', 'mxc://hs/bob'),
        member('@carol:hs', 'Carol', 'mxc://hs/carol'),
      ]),
    ).toBeNull()
    expect(dmPeerAvatarFromMembers('@me:hs', [member('@me:hs')])).toBeNull()
  })
})

describe('roomListAvatarUrl', () => {
  const cache = new Map([[`${ACCOUNT}/!dm:hs`, 'mxc://hs/bob']])

  it('prefers the room avatar over a DM peer avatar', () => {
    expect(
      roomListAvatarUrl(
        room({ room_id: '!dm:hs', avatar_url: 'mxc://hs/room' }),
        cache,
      ),
    ).toBe('mxc://hs/room')
  })

  it('uses the cached peer avatar only for unnamed rooms without a room avatar', () => {
    expect(roomListAvatarUrl(room({ room_id: '!dm:hs' }), cache)).toBe(
      'mxc://hs/bob',
    )
    expect(
      roomListAvatarUrl(room({ room_id: '!ops:hs', name: 'Ops' }), cache),
    ).toBeNull()
  })
})

describe('roomTitle', () => {
  const cache = new Map([[`${ACCOUNT}/!dm:hs`, 'Bob']])

  it('prefers name, then alias, then cached member title, then room id', () => {
    expect(
      roomTitle(
        room({ room_id: '!r:hs', name: 'Ops', canonical_alias: '#o:hs' }),
        cache,
      ),
    ).toBe('Ops')
    expect(
      roomTitle(room({ room_id: '!r:hs', canonical_alias: '#o:hs' }), cache),
    ).toBe('#o:hs')
    expect(roomTitle(room({ room_id: '!dm:hs' }), cache)).toBe('Bob')
    expect(roomTitle(room({ room_id: '!new:hs' }), cache)).toBe('!new:hs')
  })
})

describe('sortRooms (ADRs 0038/0042)', () => {
  const alpha = room({ room_id: '!a:hs', name: 'alpha', last_activity_ts: 10 })
  const beta = room({ room_id: '!b:hs', name: 'Beta', last_activity_ts: 30 })
  const gamma = room({ room_id: '!c:hs', name: 'gamma', last_activity_ts: 20 })
  const title = (r: RoomDto) => r.name ?? r.room_id

  it('recent puts newest first; oldest inverts', () => {
    expect(
      sortRooms([alpha, beta, gamma], [], 'recent', title).map(title),
    ).toEqual(['Beta', 'gamma', 'alpha'])
    expect(
      sortRooms([alpha, beta, gamma], [], 'oldest', title).map(title),
    ).toEqual(['alpha', 'gamma', 'Beta'])
  })

  it('az/za compare the rendered title case-insensitively', () => {
    expect(sortRooms([gamma, beta, alpha], [], 'az', title).map(title)).toEqual(
      ['alpha', 'Beta', 'gamma'],
    )
    expect(sortRooms([alpha, beta, gamma], [], 'za', title).map(title)).toEqual(
      ['gamma', 'Beta', 'alpha'],
    )
  })

  it('pinned rooms stay on top in pin order under every sort mode', () => {
    const pinned = [roomKey(gamma), roomKey(alpha)] // gamma pinned most recently
    for (const sort of ['recent', 'oldest', 'az', 'za'] as const) {
      const sorted = sortRooms([alpha, beta, gamma], pinned, sort, title)
      expect(sorted.slice(0, 2).map(title)).toEqual(['gamma', 'alpha'])
      expect(sorted[2]).toBe(beta)
    }
  })

  it('does not mutate its input', () => {
    const input = [beta, alpha]
    sortRooms(input, [], 'az', title)
    expect(input).toEqual([beta, alpha])
  })
})

describe('filterRooms (ADR 0042)', () => {
  const dm = room({ room_id: '!dm:hs', last_activity_ts: 1 })
  const ops = room({
    room_id: '!ops:hs',
    name: 'Ops',
    topic: 'incident channel',
    last_activity_ts: 2,
  })
  const lounge = room({
    room_id: '!lounge:hs',
    canonical_alias: '#lounge:hs',
    last_activity_ts: 3,
  })
  const rooms = [dm, ops, lounge]
  const titles = new Map([[roomKey(dm), 'Bob']])
  const context = {
    hasUnread: (key: string) => key === roomKey(ops),
    isPinned: (key: string) => key === roomKey(lounge),
    title: (r: RoomDto) => roomTitle(r, titles),
  }

  it('applies each category', () => {
    expect(filterRooms(rooms, { kind: 'all' }, context)).toEqual(rooms)
    expect(filterRooms(rooms, { kind: 'dms' }, context)).toEqual([dm])
    expect(filterRooms(rooms, { kind: 'groups' }, context)).toEqual([
      ops,
      lounge,
    ])
    expect(filterRooms(rooms, { kind: 'unread' }, context)).toEqual([ops])
    expect(filterRooms(rooms, { kind: 'favorites' }, context)).toEqual([lounge])
  })

  it('name filter matches name, alias, topic, room id, and rendered title', () => {
    const byName = (query: string) =>
      filterRooms(rooms, { kind: 'name', query }, context)
    expect(byName('ops')).toEqual([ops])
    expect(byName('#LOUNGE')).toEqual([lounge])
    expect(byName('incident')).toEqual([ops])
    expect(byName('!dm')).toEqual([dm])
    expect(byName('bob')).toEqual([dm]) // member-derived title
    expect(byName('  ')).toEqual(rooms) // blank query filters nothing
  })
})

describe('localpart', () => {
  it('drops the homeserver part', () => {
    expect(localpart('@adam:bostoncoop.net')).toBe('@adam')
    expect(localpart('@a:hs')).toBe('@a')
  })

  it('returns a user id with no server part unchanged', () => {
    expect(localpart('@adam')).toBe('@adam')
    expect(localpart('')).toBe('')
  })
})

describe('accountLabels', () => {
  it('shortens to localparts when they are unambiguous', () => {
    const labels = accountLabels([
      ['a1', '@adam:bostoncoop.net'],
      ['a2', '@a:bostoncoop.net'],
    ])
    expect(labels.get('a1')).toBe('@adam')
    expect(labels.get('a2')).toBe('@a')
  })

  it('keeps full ids when two accounts share a localpart', () => {
    // Same localpart on different homeservers: a shortened row could not tell
    // them apart, which is worse than a long one.
    const labels = accountLabels([
      ['a1', '@adam:a.net'],
      ['a2', '@adam:b.net'],
    ])
    expect(labels.get('a1')).toBe('@adam:a.net')
    expect(labels.get('a2')).toBe('@adam:b.net')
  })

  it('handles a single account', () => {
    expect(accountLabels([['a1', '@adam:a.net']]).get('a1')).toBe('@adam')
    expect(accountLabels([]).size).toBe(0)
  })
})
