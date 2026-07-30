import type { RoomEntryResult } from './stores/rooms'

/**
 * Human-readable failure for a join attempt. `target` names what the user asked
 * for — a typed room ID, a directory entry, or a relationship link's label.
 */
export function joinRoomError(
  target: string,
  result: Extract<RoomEntryResult, { ok: false }>,
): string {
  switch (result.code) {
    case 'not_found':
      return `Could not find ${target}. Check the room ID or alias. Room names are not join addresses; use a room ID, canonical alias, or Matrix link.`
    case 'forbidden':
      return `Could not join ${target}. This account is not allowed to join that room. You may need an invite, a knockable room, or a Matrix link from a member.`
    case 'bad_request':
      return `Could not join ${target}. Check that the room ID, alias, or Matrix link is valid.`
    case 'service_unavailable':
      return `Could not join ${target}. The selected account is not connected; try again after Axon reconnects.`
    default:
      return `Could not join ${target}: ${result.message}`
  }
}
