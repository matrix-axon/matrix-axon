/**
 * The loaded part of a space's paged `space/hierarchy` (ADR 0133 decision 6),
 * and how a new page or a re-read changes it. Pure: the room-details store
 * owns the requests, the abort handles and when each read happens.
 */
import type { components } from '../api/schema'

type Schemas = components['schemas']
export type SpaceHierarchyPage = Schemas['SpaceHierarchyPage']
export type SpaceHierarchyChildDto = Schemas['SpaceHierarchyChildDto']
export type SpaceDiscoveryDto = Schemas['SpaceDiscoveryDto']

/** Rows the first page asks for, and the least a re-read asks for. */
export const HIERARCHY_PAGE = 50
/** The server's largest page (`SPACE_HIERARCHY_MAX_PAGE`). */
export const HIERARCHY_MAX_PAGE = 500

export interface HierarchyList {
  /** Loaded children in MSC1772 order, each room once. */
  children: readonly SpaceHierarchyChildDto[]
  /** The space's child links, counted with the latest page. */
  totalChildren: number
  /** Continues after the last loaded row; `null` once the end is loaded. */
  nextCursor: string | null
  /** The space's crawl, from the latest page: the source of truth. */
  discovery: SpaceDiscoveryDto
}

/**
 * How a re-read treats a list past `HIERARCHY_MAX_PAGE` rows. A frame (or a
 * link change in the timeline) follows a change the client has just been
 * told about, so it keeps the loaded tail when it safely can; a reconnect
 * may have missed any number of changes, so it drops the tail.
 */
export type RereadMode = 'frame' | 'reconnect'

/** Each room once, keeping its first position: a link whose `order` changes mid-walk can appear twice. */
function unique(
  rows: readonly SpaceHierarchyChildDto[],
  seen = new Set<string>(),
): SpaceHierarchyChildDto[] {
  const kept: SpaceHierarchyChildDto[] = []
  for (const row of rows) {
    if (seen.has(row.room_id)) continue
    seen.add(row.room_id)
    kept.push(row)
  }
  return kept
}

/** A list that is exactly `page`: a first read, or a re-read that covers everything loaded. */
export function listFromPage(page: SpaceHierarchyPage): HierarchyList {
  return {
    children: unique(page.children),
    totalChildren: page.total_children,
    nextCursor: page.next_cursor ?? null,
    discovery: page.discovery,
  }
}

/** `list` followed by the next page ("Load more"). */
export function appendPage(
  list: HierarchyList,
  page: SpaceHierarchyPage,
): HierarchyList {
  const seen = new Set(list.children.map((row) => row.room_id))
  return {
    children: [...list.children, ...unique(page.children, seen)],
    totalChildren: page.total_children,
    nextCursor: page.next_cursor ?? null,
    discovery: page.discovery,
  }
}

/** The `limit` a re-read from the start asks for: everything loaded, within the page bounds. */
export function rereadLimit(list: HierarchyList | null): number {
  const loaded = list?.children.length ?? 0
  return Math.min(HIERARCHY_MAX_PAGE, Math.max(HIERARCHY_PAGE, loaded))
}

/**
 * `list` after a re-read from the start returned `page` (decision 6).
 *
 * With `HIERARCHY_MAX_PAGE` rows or fewer loaded, `page` covers all of them,
 * so it replaces the list and its cursor becomes the list's. Past that, a
 * reconnect re-read replaces the list with `page` too, dropping the tail.
 *
 * A frame re-read past that keeps the loaded tail and the tail's cursor (a
 * keyset position, so "Load more" still continues after the tail's last
 * child), dropping any tail row the refreshed rows now contain. It does so
 * only if the refreshed rows include every room of the first
 * `HIERARCHY_MAX_PAGE` loaded: a child linked into those positions pushes
 * the old rows near the boundary out of the response while the tail starts
 * after them, so they would be in neither part. Rows carry no `origin_ts`,
 * so this room-ID check is all the client can compare.
 */
export function mergeReread(
  list: HierarchyList | null,
  page: SpaceHierarchyPage,
  mode: RereadMode,
): HierarchyList {
  const refreshed = listFromPage(page)
  if (
    list === null ||
    list.children.length <= HIERARCHY_MAX_PAGE ||
    mode === 'reconnect'
  ) {
    return refreshed
  }
  const refreshedIds = new Set(refreshed.children.map((row) => row.room_id))
  const headKept = list.children
    .slice(0, HIERARCHY_MAX_PAGE)
    .every((row) => refreshedIds.has(row.room_id))
  if (!headKept) return refreshed
  const tail = list.children
    .slice(HIERARCHY_MAX_PAGE)
    .filter((row) => !refreshedIds.has(row.room_id))
  return {
    children: [...refreshed.children, ...tail],
    totalChildren: refreshed.totalChildren,
    nextCursor: list.nextCursor,
    discovery: refreshed.discovery,
  }
}

/**
 * Progress row 5: a transient crawl failure, the only one the store retries
 * on a timer (decision 7). Rows 1–4 come first in the table, so a crawl that
 * is queued or running, unsupported, or denied is not one.
 */
export function crawlRetryable(discovery: SpaceDiscoveryDto): boolean {
  return (
    discovery.state === 'idle' &&
    discovery.support !== 'unsupported' &&
    discovery.outcome === 'failed' &&
    discovery.failure !== 'forbidden' &&
    discovery.failure !== 'not_found'
  )
}

/** Progress row 6: a crawl the server paused, which one `enrich` read resumes. */
export function crawlPaused(discovery: SpaceDiscoveryDto): boolean {
  return (
    discovery.state === 'idle' &&
    discovery.support !== 'unsupported' &&
    discovery.outcome === 'in_progress'
  )
}
