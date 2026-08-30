import { expect, it } from 'vitest'
import {
  maySetRoomState,
  powerLevelCaution,
  resolvedPowerLevel,
  type PowerLevelsDto,
} from './room-power'

const ME = '@me:example.org'

function levels(extra: Partial<PowerLevelsDto> = {}): PowerLevelsDto {
  return {
    ban: 50,
    events_default: 0,
    invite: 0,
    kick: 50,
    redact: 50,
    state_default: 50,
    users_default: 0,
    users: {},
    ...extra,
  }
}

it('resolves an explicit users entry', () => {
  expect(resolvedPowerLevel(levels({ users: { [ME]: 100 } }), ME)).toBe(100)
})

it('falls back to users_default when the user is absent', () => {
  expect(resolvedPowerLevel(levels({ users_default: 25 }), ME)).toBe(25)
})

it('prefers an explicit entry over users_default, including a demotion', () => {
  const value = resolvedPowerLevel(
    levels({ users_default: 50, users: { [ME]: 0 } }),
    ME,
  )
  expect(value).toBe(0)
})

it('treats an explicit zero as a level, not as absent', () => {
  // `users[ME] ?? users_default` — a `||` here would wrongly read 0 as unset
  // and hand the user the room default instead of their real demotion.
  expect(
    resolvedPowerLevel(levels({ users_default: 50, users: { [ME]: 0 } }), ME),
  ).toBe(0)
})

it('allows editing at exactly state_default', () => {
  expect(maySetRoomState(levels({ users: { [ME]: 50 } }), ME)).toBe(true)
})

it('refuses editing below state_default', () => {
  expect(maySetRoomState(levels({ users: { [ME]: 49 } }), ME)).toBe(false)
})

it('allows editing when the room default already clears the bar', () => {
  expect(maySetRoomState(levels({ state_default: 0 }), ME)).toBe(true)
})

it('words the caution as a likelihood, not a verdict', () => {
  const message = powerLevelCaution(
    levels({ state_default: 50, users: { [ME]: 10 } }),
    ME,
  )
  expect(message).toContain('appears as 10')
  expect(message).toContain('50')
  // Never an assertion that the user lacks permission: a room-version-12
  // creator reads as `users_default` here and is in fact omnipotent (#324).
  expect(message).toContain('homeserver decides')
  expect(message).not.toMatch(/you (do not|don't|cannot|can't)/i)
})

it('reads a room-v12 creator as users_default, which is why this is a hint', () => {
  // A v12 room cannot list its creators in `users`, so the map is empty and
  // the creator resolves to the room default — 0 in practice.
  const v12 = levels({ users: {}, users_default: 0, state_default: 50 })
  expect(resolvedPowerLevel(v12, ME)).toBe(0)
  expect(maySetRoomState(v12, ME)).toBe(false)
})
