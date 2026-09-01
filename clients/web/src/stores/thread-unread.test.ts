import { describe, expect, it } from 'vitest'
import type { EventDto } from './timeline'
import { createThreadUnreadStore } from './thread-unread'

const ACCT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const ROOT = '$root:hs'

function event(
  id: string,
  ts: number,
  options: { sender?: string; root?: string | null; body?: string } = {},
): EventDto {
  return {
    account_id: ACCT,
    room_id: ROOM,
    event_id: id,
    sender: options.sender ?? '@alice:hs',
    origin_ts: ts,
    arrival_order: ts,
    type: 'm.room.message',
    state_key: null,
    body: options.body ?? `body ${id}`,
    content: { msgtype: 'm.text', body: options.body ?? `body ${id}` },
    redacted: false,
    redaction_event_id: null,
    relates_to:
      options.root === null
        ? null
        : { rel_type: 'm.thread', event_id: options.root ?? ROOT },
    sender_trust: null,
    edited: false,
    edit_count: 0,
    latest_edit_ts: null,
    reactions: null,
  } as unknown as EventDto
}

describe('createThreadUnreadStore', () => {
  it('records live thread replies but skips own replies and the open thread', () => {
    const store = createThreadUnreadStore()

    store.recordLiveEvent(event('$plain', 50, { root: null }), {
      roomTitle: 'Ops',
    })
    store.recordLiveEvent(event('$own', 100, { sender: '@me:hs' }), {
      roomTitle: 'Ops',
      ownUserId: '@me:hs',
    })
    store.recordLiveEvent(event('$open', 150), {
      roomTitle: 'Ops',
      activeThread: { accountId: ACCT, roomId: ROOM, rootEventId: ROOT },
    })
    expect(store.entries.value).toHaveLength(0)

    store.recordLiveEvent(event('$reply', 200, { body: 'new reply' }), {
      roomTitle: 'Ops',
      rootPreview: 'root body',
    })
    expect(store.count.value).toBe(1)
    expect(store.isUnread(ACCT, ROOM, ROOT)).toBe(true)
    expect(store.entries.value[0]).toMatchObject({
      latestEventId: '$reply',
      latestBody: 'new reply',
      roomTitle: 'Ops',
      rootPreview: 'root body',
    })
  })

  it('seeds from summaries only when unread and, off the room marker, recent', () => {
    const NOW = 1_700_000_000_000
    const DAY = 24 * 60 * 60_000
    const store = createThreadUnreadStore(() => NOW)
    const base = {
      accountId: ACCT,
      roomId: ROOM,
      roomTitle: 'Ops',
      threadMarker: null,
    }
    const summaryAt = (ts: number) => ({
      root_event_id: ROOT,
      latest_reply_event_id: '$reply',
      latest_reply_ts: ts,
      reply_count: 3,
    })

    // No read position at all: silent, not a guess.
    store.reconcileSummary(summaryAt(NOW - DAY), { ...base, roomMarker: null })
    expect(store.count.value).toBe(0)

    // Room marker behind a recent reply: raised.
    store.reconcileSummary(summaryAt(NOW - DAY), {
      ...base,
      roomMarker: { eventId: '$before', originTs: NOW - 2 * DAY },
    })
    expect(store.isUnread(ACCT, ROOM, ROOT)).toBe(true)

    // Room marker behind a reply older than the recency window: held back — the
    // marker is a main-timeline position, and this is the years-old-thread noise.
    store.markThreadRead(ACCT, ROOM, ROOT)
    store.reconcileSummary(summaryAt(NOW - 30 * DAY), {
      ...base,
      roomMarker: { eventId: '$before', originTs: NOW - 31 * DAY },
    })
    expect(store.isUnread(ACCT, ROOM, ROOT)).toBe(false)
  })

  it('raises a stale reply when a per-thread marker places it unread', () => {
    const NOW = 1_700_000_000_000
    const DAY = 24 * 60 * 60_000
    const store = createThreadUnreadStore(() => NOW)
    const staleTs = NOW - 90 * DAY

    store.reconcileSummary(
      {
        root_event_id: ROOT,
        latest_reply_event_id: '$reply',
        latest_reply_ts: staleTs,
        reply_count: 3,
      },
      {
        accountId: ACCT,
        roomId: ROOM,
        roomTitle: 'Ops',
        roomMarker: { eventId: '$before', originTs: staleTs - 10 * DAY },
        threadMarker: {
          roomId: ROOM,
          rootEventId: ROOT,
          eventId: '$read-through',
          originTs: staleTs - DAY,
          arrivalThrough: null,
        },
      },
    )
    // A precise read position: the age of the gap is irrelevant.
    expect(store.isUnread(ACCT, ROOM, ROOT)).toBe(true)
  })

  it('clears an entry once a marker proves the latest reply read', () => {
    const NOW = 1_700_000_000_000
    const store = createThreadUnreadStore(() => NOW)
    store.recordLiveEvent(event('$reply', NOW - 1000, { root: ROOT }), {
      roomTitle: 'Ops',
    })
    expect(store.isUnread(ACCT, ROOM, ROOT)).toBe(true)

    store.reconcileSummary(
      {
        root_event_id: ROOT,
        latest_reply_event_id: '$reply',
        latest_reply_ts: NOW - 1000,
        reply_count: 1,
      },
      {
        accountId: ACCT,
        roomId: ROOM,
        roomTitle: 'Ops',
        roomMarker: { eventId: '$after', originTs: NOW },
        threadMarker: null,
      },
    )
    expect(store.isUnread(ACCT, ROOM, ROOT)).toBe(false)
  })

  it('sorts by latest activity and clears by thread identity', () => {
    const store = createThreadUnreadStore()
    store.recordLiveEvent(event('$old', 100, { root: '$oldroot' }), {
      roomTitle: 'Ops',
    })
    store.recordLiveEvent(event('$new', 300, { root: '$newroot' }), {
      roomTitle: 'Ops',
    })

    expect(store.entries.value.map((entry) => entry.rootEventId)).toEqual([
      '$newroot',
      '$oldroot',
    ])
    store.markThreadRead(ACCT, ROOM, '$newroot')
    expect(store.entries.value.map((entry) => entry.rootEventId)).toEqual([
      '$oldroot',
    ])
  })
})
