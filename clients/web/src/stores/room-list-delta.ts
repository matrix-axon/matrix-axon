import type { components } from '../api/schema'
import { persistable } from './room-list-cache'
import { roomKey, type RoomDto } from './room-list'

export type RoomRefDto = components['schemas']['RoomRefDto']

/** The outcome of merging one delta into the server's list (ADR 0134). */
export interface RoomListMerge {
  /** The server's list as of the delta's cursor, in the server's order. */
  rooms: RoomDto[]
  /**
   * Whether the merge changed anything. Every delta repeats the rooms that
   * changed inside the server's overlap window, so most resumes return rows
   * identical to the ones held; those are not a change worth a disk write.
   */
  changed: boolean
}

/** The server's order: newest activity first, then room id. */
export function compareRoomOrder(a: RoomDto, b: RoomDto): number {
  if (a.last_activity_ts !== b.last_activity_ts) {
    return b.last_activity_ts - a.last_activity_ts
  }
  return a.room_id < b.room_id ? -1 : a.room_id > b.room_id ? 1 : 0
}

function sameRoom(a: RoomDto, b: RoomDto): boolean {
  // The persisted projection lists every field of `RoomDto`, normalized
  // (`null` for absent, `[]` for no tags), so a row read back from disk
  // compares equal to the same row fresh from the server. It doubles as the
  // definition of equality here, which is why `persistable` is checked at
  // compile time to name every field.
  return JSON.stringify(persistable(a)) === JSON.stringify(persistable(b))
}

/**
 * Merge a delta into the server's list: each changed room replaces or adds
 * its row, each removed room is dropped. Pure, and never touches `base`.
 */
export function mergeRoomListDelta(
  base: readonly RoomDto[],
  changedRooms: readonly RoomDto[],
  removed: readonly RoomRefDto[],
): RoomListMerge {
  const byKey = new Map(base.map((room) => [roomKey(room), room]))
  let changed = false
  for (const room of changedRooms) {
    const key = roomKey(room)
    const held = byKey.get(key)
    if (held === undefined || !sameRoom(held, room)) {
      changed = true
    }
    byKey.set(key, room)
  }
  for (const ref of removed) {
    if (byKey.delete(roomKey(ref))) {
      changed = true
    }
  }
  if (!changed) {
    // Keep the held rows and their order: nothing moved. A copy of the
    // array, since the result is the caller's to keep.
    return { rooms: [...base], changed }
  }
  return { rooms: [...byKey.values()].sort(compareRoomOrder), changed }
}
