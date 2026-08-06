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

  it('seeds from summaries only when newer than the room or thread marker', () => {
    const store = createThreadUnreadStore()
    const summary = {
      root_event_id: ROOT,
      latest_reply_event_id: '$reply',
      latest_reply_ts: 200,
      reply_count: 3,
    }

    store.reconcileSummary(summary, {
      accountId: ACCT,
      roomId: ROOM,
      roomTitle: 'Ops',
      roomMarker: null,
      threadMarker: null,
    })
    expect(store.count.value).toBe(0)

    store.reconcileSummary(summary, {
      accountId: ACCT,
      roomId: ROOM,
      roomTitle: 'Ops',
      roomMarker: { eventId: '$before', originTs: 100 },
      threadMarker: null,
    })
    expect(store.isUnread(ACCT, ROOM, ROOT)).toBe(true)

    store.reconcileSummary(summary, {
      accountId: ACCT,
      roomId: ROOM,
      roomTitle: 'Ops',
      roomMarker: { eventId: '$before', originTs: 100 },
      threadMarker: {
        roomId: ROOM,
        rootEventId: ROOT,
        eventId: '$reply',
        originTs: 200,
      },
    })
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
