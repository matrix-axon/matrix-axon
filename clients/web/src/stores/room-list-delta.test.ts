import { describe, expect, it } from 'vitest'
import { compareRoomOrder, mergeRoomListDelta } from './room-list-delta'
import type { RoomDto } from './room-list'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const OTHER = '6b53f7f0-0000-4000-8000-000000000002'

function room(
  room_id: string,
  last_activity_ts: number,
  extra: Partial<RoomDto> = {},
): RoomDto {
  return {
    account_id: ACCOUNT,
    account_user_id: '@me:hs',
    room_id,
    last_activity_ts,
    notification_count: 0,
    highlight_count: 0,
    is_direct: false,
    ...extra,
  } as RoomDto
}

const ids = (rooms: readonly RoomDto[]) =>
  rooms.map((r) => `${r.account_id === OTHER ? 'other:' : ''}${r.room_id}`)

describe('mergeRoomListDelta', () => {
  it('replaces changed rows, adds new ones and drops removed ones, in server order', () => {
    const base = [room('!a', 300), room('!b', 200), room('!c', 100)]
    const merged = mergeRoomListDelta(
      base,
      [room('!c', 400, { name: 'C' }), room('!d', 250)],
      [{ account_id: ACCOUNT, room_id: '!b' }],
    )
    expect(merged.changed).toBe(true)
    expect(ids(merged.rooms)).toEqual(['!c', '!a', '!d'])
    expect(merged.rooms[0].name).toBe('C')
    expect(ids(base)).toEqual(['!a', '!b', '!c'])
  })

  it('reports no change when a delta only repeats rows already held', () => {
    // The server's overlap resends recent rows. A row read back from disk has
    // `null` for absent fields and `[]` for no tags, and must still compare
    // equal to the same row as the server sends it.
    const held = room('!a', 300, { name: null, tags: [] })
    const base = [held, room('!b', 200)]
    const merged = mergeRoomListDelta(base, [room('!a', 300)], [])
    expect(merged.changed).toBe(false)
    expect(merged.rooms).toEqual(base)
    expect(merged.rooms[0]).toBe(held)
  })

  it('counts a removal of a room it does not hold as no change', () => {
    const base = [room('!a', 300)]
    const merged = mergeRoomListDelta(
      base,
      [],
      [{ account_id: ACCOUNT, room_id: '!gone' }],
    )
    expect(merged.changed).toBe(false)
    expect(ids(merged.rooms)).toEqual(['!a'])
  })

  it('keys rooms by account, so one account leaving does not drop the other copy', () => {
    const base = [room('!a', 300), room('!a', 300, { account_id: OTHER })]
    const merged = mergeRoomListDelta(
      base,
      [],
      [{ account_id: OTHER, room_id: '!a' }],
    )
    expect(ids(merged.rooms)).toEqual(['!a'])
    expect(merged.rooms[0].account_id).toBe(ACCOUNT)
  })

  it('sees a changed unread count, tag or DM flag as a change', () => {
    const base = [room('!a', 300)]
    for (const changed of [
      room('!a', 300, { notification_count: 2 }),
      room('!a', 300, { tags: [{ name: 'm.favourite' }] }),
      room('!a', 300, { is_direct: true }),
    ]) {
      expect(mergeRoomListDelta(base, [changed], []).changed).toBe(true)
    }
  })
})

describe('compareRoomOrder', () => {
  it('sorts newest activity first, then by room id', () => {
    const rooms = [room('!b', 100), room('!a', 100), room('!c', 200)]
    expect(ids([...rooms].sort(compareRoomOrder))).toEqual(['!c', '!a', '!b'])
  })
})
