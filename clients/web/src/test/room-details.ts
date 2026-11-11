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

export type SpaceHierarchyPage = Schemas['SpaceHierarchyPage']
export type SpaceHierarchyChildDto = Schemas['SpaceHierarchyChildDto']
export type SpaceDiscoveryDto = Schemas['SpaceDiscoveryDto']
export type DiscoveredRoomDto = Schemas['DiscoveredRoomDto']
export type RoomDiscoveryDto = Schemas['RoomDiscoveryDto']

/** A space's crawl that has ended `complete`: nothing beyond "Showing N of T". */
export function spaceDiscovery(
  overrides: Partial<SpaceDiscoveryDto> = {},
): SpaceDiscoveryDto {
  return {
    state: 'idle',
    support: 'supported',
    outcome: 'complete',
    failure: null,
    next_attempt_at: null,
    completed_at: 1_700_000_000_000,
    expires_at: Date.now() + 6 * 3_600_000,
    started_at: 1_700_000_000_000,
    page_limit: 50,
    pages: 1,
    rooms_cached: 0,
    ...overrides,
  }
}

/** An `available` summary from the space directory. */
export function discoveredRoom(
  overrides: Partial<DiscoveredRoomDto> = {},
): DiscoveredRoomDto {
  return {
    status: 'available',
    source: 'hierarchy',
    observed_at: 1_700_000_000_000,
    expires_at: Date.now() + 6 * 3_600_000,
    withheld: [],
    name: 'Salmon count',
    topic: 'Counting fish',
    canonical_alias: '#salmon-count:hs',
    num_joined_members: 4,
    join_rule: 'public',
    encryption: null,
    room_version: '11',
    ...overrides,
  }
}

/** One hierarchy row: an unjoined child with no synced fields and no summary. */
export function hierarchyChild(
  roomId: string,
  overrides: Partial<SpaceHierarchyChildDto> = {},
): SpaceHierarchyChildDto {
  return {
    room_id: roomId,
    parent_room_id: '!space:hs',
    via: ['hs'],
    suggested: false,
    joined: false,
    discovered: null,
    ...overrides,
  } as SpaceHierarchyChildDto
}

export function hierarchyPage(
  children: readonly SpaceHierarchyChildDto[],
  overrides: Partial<SpaceHierarchyPage> = {},
): SpaceHierarchyPage {
  return {
    room_id: '!space:hs',
    children: [...children],
    total_children: children.length,
    next_cursor: null,
    discovery: spaceDiscovery(),
    ...overrides,
  }
}

/** `/discovery` for an eligible room the server can summarise. */
export function roomDiscovery(
  roomId: string,
  overrides: Partial<RoomDiscoveryDto> = {},
): RoomDiscoveryDto {
  return {
    room_id: roomId,
    eligible: true,
    support: 'supported',
    refresh: { state: 'idle' },
    discovered: discoveredRoom({ source: 'summary' }),
    ...overrides,
  }
}
