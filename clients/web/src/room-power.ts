import type { components } from './api/schema'

export type PowerLevelsDto = components['schemas']['PowerLevelsDto']

/**
 * This account's power level in a room, by the standard Matrix resolution: an
 * explicit `users` entry if there is one, otherwise the room's `users_default`.
 *
 * **This can understate the truth.** From room version 12 a room's creators
 * hold an effectively infinite power level and cannot appear in `users` at
 * all, so a creator resolves here to `users_default` — usually 0. `ruma`
 * models this (`RoomPowerLevels::for_user` returns `Infinite` for a
 * privileged creator), but Axon's `PowerLevelsDto` flattens the resolved
 * levels and drops the creator set, so a client cannot reconstruct it.
 */
export function resolvedPowerLevel(
  levels: PowerLevelsDto,
  userId: string,
): number {
  return levels.users[userId] ?? levels.users_default
}

/**
 * Whether the role thresholds *suggest* this account may edit the room's
 * name, topic and avatar. All three are state events with no threshold of
 * their own, so `state_default` covers them.
 *
 * A hint, never a decision. It is known to be wrong in both directions:
 *
 * - **False negative:** a room-version-12 creator has infinite power but
 *   resolves here to `users_default` (see `resolvedPowerLevel`).
 * - **False positive / negative:** `PowerLevelsDto` carries no `events` map,
 *   so a room overriding one of these event types specifically is invisible.
 *
 * Callers must therefore never use it to *block* editing — only to warn.
 * The homeserver's 403 is the authority.
 */
export function maySetRoomState(
  levels: PowerLevelsDto,
  userId: string,
): boolean {
  return resolvedPowerLevel(levels, userId) >= levels.state_default
}

/**
 * The caution shown when the thresholds suggest a refusal. Worded as a
 * likelihood rather than a verdict, because of the creator case above:
 * telling a room's owner they lack permission would be simply false.
 */
export function powerLevelCaution(
  levels: PowerLevelsDto,
  userId: string,
): string {
  return (
    `Your room power level appears as ` +
    `${resolvedPowerLevel(levels, userId)}, below ` +
    `${levels.state_default} normally needed to edit room info. ` +
    `Some rooms grant their creator full rights regardless, so you can still ` +
    `try — homeserver decides.`
  )
}
