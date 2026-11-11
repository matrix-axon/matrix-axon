/**
 * `/metadata` and `/homeserver_details` fixtures (ADR 0133). The defaults
 * describe an ordinary encrypted, invite-only room whose every snapshot is
 * cached; tests override one snapshot or detail at a time.
 */
import type { components } from '../api/schema'

type Schemas = components['schemas']
export type RoomMetadataDto = Schemas['RoomMetadataDto']
export type RoomHomeserverDetailsDto = Schemas['RoomHomeserverDetailsDto']

const IDLE = { state: 'idle' as const }

/** A cached snapshot with `content`, as the server reports one. */
export function snapshot<C>(content: C, extra: Record<string, unknown> = {}) {
  return {
    status: 'available' as const,
    content,
    invalid_fields: [] as string[],
    event_id: '$state',
    sender: '@creator:hs',
    origin_ts: 1_700_000_000_000,
    redacted: null,
    absence_confirmed_at: null,
    absence_check: null,
    ...extra,
  }
}

/** A snapshot with no cached tuple: `unknown`, nothing confirmed, nothing queued. */
export function unknownSnapshot(extra: Record<string, unknown> = {}) {
  return {
    status: 'unknown' as const,
    content: null,
    invalid_fields: [] as string[],
    absence_confirmed_at: null,
    absence_check: IDLE,
    ...extra,
  }
}

export function metadataDto(
  overrides: Partial<Record<keyof RoomMetadataDto, unknown>> = {},
): RoomMetadataDto {
  return {
    aliases: snapshot({ alias: '#room:hs', alt_aliases: ['#alt:hs'] }),
    creation: snapshot({ room_version: '11', creator: null }),
    encryption: snapshot({ algorithm: 'm.megolm.v1.aes-sha2' }),
    guest_access: snapshot({ guest_access: 'forbidden' }),
    history_visibility: snapshot({ history_visibility: 'shared' }),
    join_rules: snapshot({ join_rule: 'invite' }),
    power_levels: unknownSnapshot(),
    server_acl: unknownSnapshot({ absence_confirmed_at: 1_700_000_000_000 }),
    ...overrides,
  } as unknown as RoomMetadataDto
}

export function homeserverDetailsDto(
  overrides: Partial<Record<keyof RoomHomeserverDetailsDto, unknown>> = {},
): RoomHomeserverDetailsDto {
  return {
    local_aliases: {
      status: 'available',
      aliases: ['#local:hs'],
      fetched_at: 1_700_000_000_000,
      expires_at: Date.now() + 15 * 60_000,
      refresh: IDLE,
    },
    directory_visibility: {
      status: 'available',
      visibility: 'private',
      fetched_at: 1_700_000_000_000,
      expires_at: Date.now() + 15 * 60_000,
      refresh: IDLE,
    },
    ...overrides,
  } as unknown as RoomHomeserverDetailsDto
}
