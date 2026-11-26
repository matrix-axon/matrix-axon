import type { components } from '../api/schema'
import { roomKey, type RoomDto } from './room-list'

export type SpaceGraphLink = components['schemas']['SpaceGraphLinkDto']
export type DiscoveredMembership =
  components['schemas']['DiscoveredSpaceMembershipDto']
export type SpaceChildDto = components['schemas']['SpaceChildDto']

export type SpaceGraph =
  | { links: readonly SpaceGraphLink[] }
  | {
      legacy: ReadonlyMap<string, readonly SpaceChildDto[]>
      errors: ReadonlyMap<string, string>
    }

export interface SpaceProjection {
  /** Advertised direct parents for Room Information, before cutting cycles. */
  parents: ReadonlyMap<string, ReadonlyMap<string, readonly string[]>>
  /** Joined space links in Matrix sibling order. */
  subspaces: ReadonlyMap<string, readonly string[]>
  /** Joined spaces that can appear below a root. */
  nested: ReadonlySet<string>
  /** Reverse links for walking up to every space containing a chat room. */
  ancestors: ReadonlyMap<string, ReadonlySet<string>>
  /** Forward links for walking down from each selected space. */
  members: ReadonlyMap<string, ReadonlySet<string>>
}

type CatalogRoom = Pick<RoomDto, 'account_id' | 'room_id' | 'room_type'>

function compareText(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0
}

function compareSiblings(left: SpaceGraphLink, right: SpaceGraphLink): number {
  if (left.order != null && right.order == null) return -1
  if (left.order == null && right.order != null) return 1
  return (
    compareText(left.order ?? '', right.order ?? '') ||
    left.origin_ts - right.origin_ts ||
    compareText(left.child_room_id, right.child_room_id)
  )
}

function addLink(map: Map<string, Set<string>>, from: string, to: string) {
  const links = map.get(from) ?? new Set<string>()
  links.add(to)
  map.set(from, links)
}

function nestedSpaces(input: ReadonlyMap<string, readonly string[]>) {
  const nested = new Set<string>()
  const incoming = new Set([...input.values()].flat())
  const keys = [...input.keys()].sort(compareText)
  const visited = new Set<string>()
  const path = new Set<string>()
  // Start with natural roots. Then visit components with no natural root so
  // a loop cannot strand all its spaces. The explicit stack supports deep trees.
  for (const root of [...keys.filter((key) => !incoming.has(key)), ...keys]) {
    if (visited.has(root)) continue
    visited.add(root)
    path.add(root)
    const frames = [{ key: root, children: (input.get(root) ?? []).values() }]
    while (frames.length > 0) {
      const frame = frames[frames.length - 1]
      const next = frame.children.next()
      if (next.done === true) {
        path.delete(frame.key)
        frames.pop()
        continue
      }
      const child = next.value
      if (path.has(child)) continue
      nested.add(child)
      if (visited.has(child)) continue
      visited.add(child)
      path.add(child)
      frames.push({ key: child, children: (input.get(child) ?? []).values() })
    }
  }
  return nested
}

export function projectSpaces(
  catalog: ReadonlyMap<string, CatalogRoom>,
  graphs: ReadonlyMap<string, SpaceGraph>,
  discovered: ReadonlyMap<string, readonly DiscoveredMembership[]>,
  now: number,
): SpaceProjection {
  const parents = new Map<string, Map<string, readonly string[]>>()
  const members = new Map<string, Set<string>>()
  const ancestors = new Map<string, Set<string>>()
  const branches = new Map<string, string[]>()

  const joinedLink = (parent: string, child: string) =>
    parent !== child &&
    catalog.get(parent)?.room_type === 'm.space' &&
    catalog.has(child)
  const addMembership = (parent: string, child: string) => {
    addLink(members, parent, child)
    addLink(ancestors, child, parent)
  }
  const addAdvertisedLink = (
    parent: string,
    child: string,
    via: readonly string[],
  ) => {
    const entries = parents.get(child) ?? new Map()
    entries.set(parent, via)
    parents.set(child, entries)
  }

  for (const [accountId, graph] of graphs) {
    const key = (roomId: string) =>
      roomKey({ account_id: accountId, room_id: roomId })
    if ('legacy' in graph) {
      for (const [parent, children] of graph.legacy) {
        for (const child of children) {
          const childKey = key(child.room_id)
          if (!joinedLink(parent, childKey)) continue
          addAdvertisedLink(parent, childKey, child.via)
          if (catalog.get(childKey)?.room_type !== 'm.space') {
            addMembership(parent, childKey)
          }
        }
      }
      continue
    }
    const advertised = new Map<string, Map<string, SpaceGraphLink>>()
    for (const link of graph.links) {
      // Verified claims can inform parent navigation. The advertised child
      // event supplies sidebar membership and Matrix sibling order.
      if (link.source !== 'child') continue
      const parent = key(link.parent_room_id)
      const child = key(link.child_room_id)
      if (!joinedLink(parent, child)) continue
      addAdvertisedLink(parent, child, [])
      addMembership(parent, child)
      if (catalog.get(child)?.room_type === 'm.space') {
        const siblings = advertised.get(parent) ?? new Map()
        siblings.set(child, link)
        advertised.set(parent, siblings)
      }
    }
    for (const [parent, siblings] of advertised) {
      branches.set(
        parent,
        [...siblings.values()]
          .sort(compareSiblings)
          .map((link) => key(link.child_room_id)),
      )
    }
  }
  for (const [accountId, memberships] of discovered) {
    for (const membership of memberships) {
      if (membership.expires_at <= now) continue
      // These pairs carry no intermediate path or sibling order.
      const parent = roomKey({
        account_id: accountId,
        room_id: membership.space_id,
      })
      const child = roomKey({
        account_id: accountId,
        room_id: membership.room_id,
      })
      if (joinedLink(parent, child)) addMembership(parent, child)
    }
  }
  return {
    parents,
    members,
    ancestors,
    subspaces: branches,
    nested: nestedSpaces(branches),
  }
}

/** Visit each reachable key once, even across shared or cyclic paths. */
export function reachableKeys(
  start: string,
  links: ReadonlyMap<string, ReadonlySet<string>>,
): Set<string> {
  const seen = new Set([start])
  // Set iteration also visits entries added during the walk.
  for (const key of seen) {
    for (const next of links.get(key) ?? []) seen.add(next)
  }
  seen.delete(start)
  return seen
}
