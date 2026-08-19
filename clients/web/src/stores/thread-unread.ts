import { computed, signal, type ReadonlySignal } from '@preact/signals'
import type { ReadMarker, ThreadReadMarker } from './device-state'
import { threadRootId, type ThreadSummaryDto } from './threads'
import type { EventDto } from './timeline'

const SEP = '\0'

export interface ActiveThread {
  accountId: string
  roomId: string
  rootEventId: string
}

export interface ThreadUnreadEntry {
  accountId: string
  roomId: string
  rootEventId: string
  latestEventId: string
  latestTs: number
  roomTitle: string
  rootPreview: string | null
  latestSender: string | null
  latestBody: string | null
}

export interface ThreadUnreadStore {
  entries: ReadonlySignal<readonly ThreadUnreadEntry[]>
  count: ReadonlySignal<number>
  isUnread(accountId: string, roomId: string, rootEventId: string): boolean
  recordLiveEvent(
    event: EventDto,
    context: {
      roomTitle: string
      rootPreview?: string | null
      ownUserId?: string | null
      activeThread?: ActiveThread | null
    },
  ): void
  reconcileSummary(
    summary: ThreadSummaryDto,
    context: {
      accountId: string
      roomId: string
      roomTitle: string
      rootPreview?: string | null
      roomMarker: ReadMarker | null
      threadMarker: ThreadReadMarker | null
    },
  ): void
  markThreadRead(accountId: string, roomId: string, rootEventId: string): void
}

export function threadUnreadKey(
  accountId: string,
  roomId: string,
  rootEventId: string,
): string {
  return `${accountId}${SEP}${roomId}${SEP}${rootEventId}`
}

function isSameThread(
  active: ActiveThread | null | undefined,
  event: EventDto,
  rootEventId: string,
): boolean {
  return (
    active !== null &&
    active !== undefined &&
    active.accountId === event.account_id &&
    active.roomId === event.room_id &&
    active.rootEventId === rootEventId
  )
}

function sortedEntries(
  entries: ReadonlyMap<string, ThreadUnreadEntry>,
): ThreadUnreadEntry[] {
  return [...entries.values()].sort(
    (a, b) =>
      b.latestTs - a.latestTs ||
      a.roomTitle.localeCompare(b.roomTitle) ||
      a.rootEventId.localeCompare(b.rootEventId),
  )
}

function sameEntry(
  a: ThreadUnreadEntry | undefined,
  b: ThreadUnreadEntry,
): boolean {
  return (
    a !== undefined &&
    a.accountId === b.accountId &&
    a.roomId === b.roomId &&
    a.rootEventId === b.rootEventId &&
    a.latestEventId === b.latestEventId &&
    a.latestTs === b.latestTs &&
    a.roomTitle === b.roomTitle &&
    a.rootPreview === b.rootPreview &&
    a.latestSender === b.latestSender &&
    a.latestBody === b.latestBody
  )
}

export function createThreadUnreadStore(): ThreadUnreadStore {
  const byKey = signal<ReadonlyMap<string, ThreadUnreadEntry>>(new Map())

  function remove(key: string): void {
    if (!byKey.value.has(key)) {
      return
    }
    const next = new Map(byKey.value)
    next.delete(key)
    byKey.value = next
  }

  function upsert(entry: ThreadUnreadEntry): void {
    const key = threadUnreadKey(
      entry.accountId,
      entry.roomId,
      entry.rootEventId,
    )
    const existing = byKey.value.get(key)
    const nextEntry =
      existing === undefined
        ? entry
        : {
            ...existing,
            ...entry,
            rootPreview: entry.rootPreview ?? existing.rootPreview,
            latestSender: entry.latestSender ?? existing.latestSender,
            latestBody: entry.latestBody ?? existing.latestBody,
          }
    if (sameEntry(existing, nextEntry)) {
      return
    }
    byKey.value = new Map(byKey.value).set(key, nextEntry)
  }

  return {
    entries: computed(() => sortedEntries(byKey.value)),
    count: computed(() => byKey.value.size),

    isUnread(accountId, roomId, rootEventId) {
      return byKey.value.has(threadUnreadKey(accountId, roomId, rootEventId))
    },

    recordLiveEvent(event, context) {
      const rootEventId = threadRootId(event)
      if (
        rootEventId === null ||
        event.sender === context.ownUserId ||
        isSameThread(context.activeThread, event, rootEventId)
      ) {
        return
      }
      upsert({
        accountId: event.account_id,
        roomId: event.room_id,
        rootEventId,
        latestEventId: event.event_id,
        latestTs: event.origin_ts,
        roomTitle: context.roomTitle,
        rootPreview: context.rootPreview ?? null,
        latestSender: event.sender,
        latestBody: event.body ?? null,
      })
    },

    reconcileSummary(summary, context) {
      const latestTs = summary.latest_reply_ts ?? null
      const latestEventId = summary.latest_reply_event_id ?? null
      const key = threadUnreadKey(
        context.accountId,
        context.roomId,
        summary.root_event_id,
      )
      if (latestTs === null || latestEventId === null) {
        remove(key)
        return
      }
      const markerTs =
        context.threadMarker?.originTs ?? context.roomMarker?.originTs ?? null
      if (markerTs === null) {
        return
      }
      if (latestTs <= markerTs) {
        remove(key)
        return
      }
      upsert({
        accountId: context.accountId,
        roomId: context.roomId,
        rootEventId: summary.root_event_id,
        latestEventId,
        latestTs,
        roomTitle: context.roomTitle,
        rootPreview: context.rootPreview ?? null,
        latestSender: null,
        latestBody: null,
      })
    },

    markThreadRead(accountId, roomId, rootEventId) {
      remove(threadUnreadKey(accountId, roomId, rootEventId))
    },
  }
}
