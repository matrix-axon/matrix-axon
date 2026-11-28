import { describe, expect, it } from 'vitest'
import type { ThreadReadMarker } from '../stores/device-state'
import type { ThreadSummaryDto } from '../stores/threads'
import {
  computeRoomReceipt,
  summaryLooksUnread,
  foldedRedactions,
  type ReceiptCandidate,
} from './room-receipt'

const ROOM = '!room:hs'
const ROOT_A = '$a'
const ROOT_B = '$b'

const ev = (
  id: string,
  arrival_order: number,
  threadRoot: string | null = null,
): ReceiptCandidate => ({ event_id: id, arrival_order, threadRoot })

const summary = (root: string, latestTs: number): ThreadSummaryDto =>
  ({
    root_event_id: root,
    reply_count: 1,
    latest_reply_event_id: `${root}-latest`,
    latest_reply_ts: latestTs,
  }) as unknown as ThreadSummaryDto

const marker = (
  root: string,
  originTs: number,
  arrivalThrough: number | null,
): ThreadReadMarker => ({
  roomId: ROOM,
  rootEventId: root,
  eventId: `${root}-read`,
  originTs,
  arrivalThrough,
})

/** Everything loaded, every thread read, nothing pending — the open case. */
function caughtUp(
  overrides: Partial<Parameters<typeof computeRoomReceipt>[0]> = {},
) {
  return {
    displayed: [ev('$main', 100)],
    loaded: [ev('$main', 100), ev('$a1', 110, ROOT_A)],
    openThread: null,
    threads: [
      { summary: summary(ROOT_A, 50), marker: marker(ROOT_A, 50, 110) },
    ],
    roomMarker: null,
    storesLoaded: true,
    knownUnreadCutoff: null,
    ...overrides,
  }
}

describe('computeRoomReceipt', () => {
  it('extends over a thread whose marker covers it', () => {
    const { target, blocker } = computeRoomReceipt(caughtUp())
    expect(target?.event_id).toBe('$a1')
    expect(blocker).toBeNull()
  })

  it('stops below a reply no marker covers', () => {
    const result = computeRoomReceipt(
      caughtUp({
        loaded: [
          ev('$main', 100),
          ev('$a1', 110, ROOT_A),
          ev('$b1', 120, ROOT_B),
        ],
        threads: [
          { summary: summary(ROOT_A, 50), marker: marker(ROOT_A, 50, 110) },
          // Thread B is loaded but has no marker: unknown, not read.
          { summary: summary(ROOT_B, 50), marker: null },
        ],
        roomMarker: { eventId: '$main', originTs: 90 },
      }),
    )
    expect(result.blocker).toBe(120)
    expect(result.target?.event_id).toBe('$a1')
  })

  it('claims nothing above the main timeline while a store is still loading', () => {
    const { target, blocker } = computeRoomReceipt(
      caughtUp({ storesLoaded: false }),
    )
    expect(target?.event_id).toBe('$main')
    expect(blocker).toBe(101)
  })

  it('claims nothing above the main timeline while a thread is known unread', () => {
    const { target } = computeRoomReceipt(caughtUp({ knownUnreadCutoff: 42 }))
    expect(target?.event_id).toBe('$main')
  })

  it('treats a summary newer than every read position as unread', () => {
    // The one signal that sees replies the client has not loaded.
    const { target } = computeRoomReceipt(
      caughtUp({
        threads: [
          { summary: summary(ROOT_A, 999), marker: marker(ROOT_A, 50, 110) },
        ],
      }),
    )
    expect(target?.event_id).toBe('$main')
  })

  it('does not treat the open thread as an obstruction to itself', () => {
    const { blocker, target } = computeRoomReceipt(
      caughtUp({
        openThread: ROOT_A,
        loaded: [
          ev('$main', 100),
          ev('$a1', 110, ROOT_A),
          ev('$b1', 120, ROOT_B),
        ],
        threads: [
          { summary: summary(ROOT_A, 50), marker: marker(ROOT_A, 50, 110) },
          { summary: summary(ROOT_B, 50), marker: marker(ROOT_B, 50, 120) },
        ],
      }),
    )
    // `$a1` belongs to the open thread, so it is the panel's to claim and not a
    // bound on it; `$b1` is read, so nothing blocks.
    expect(blocker).toBeNull()
    expect(target?.event_id).toBe('$b1')
  })

  it('shuts the extension while the open thread is itself unread', () => {
    const { blocker, target } = computeRoomReceipt(
      caughtUp({
        openThread: ROOT_A,
        threads: [{ summary: summary(ROOT_A, 50), marker: null }],
      }),
    )
    // Not a special case: an unread thread is an unread thread, even the one on
    // screen. The panel's own read effect writes its marker, and the render
    // after that reopens the extension — one frame later, which is why the room
    // still closes out.
    expect(blocker).toBe(101)
    expect(target?.event_id).toBe('$main')
  })

  it('falls back to resolving a legacy marker in the loaded set', () => {
    const { target } = computeRoomReceipt(
      caughtUp({
        loaded: [ev('$main', 100), ev(`${ROOT_A}-read`, 110, ROOT_A)],
        // Written before `arrivalThrough` existed.
        threads: [
          { summary: summary(ROOT_A, 50), marker: marker(ROOT_A, 50, null) },
        ],
      }),
    )
    expect(target?.event_id).toBe(`${ROOT_A}-read`)
  })

  it('treats a legacy marker whose event is not loaded as no evidence', () => {
    const result = computeRoomReceipt(
      caughtUp({
        threads: [
          { summary: summary(ROOT_A, 50), marker: marker(ROOT_A, 50, null) },
        ],
      }),
    )
    expect(result.blocker).toBe(110)
    expect(result.target?.event_id).toBe('$main')
  })
})

describe('summaryLooksUnread', () => {
  it('is false for a thread with no replies', () => {
    const empty = { ...summary(ROOT_A, 0), latest_reply_ts: null }
    expect(
      summaryLooksUnread(empty as unknown as ThreadSummaryDto, null, 'unread'),
    ).toBe(false)
  })

  it('splits on the unknown case, which is the whole point of the parameter', () => {
    expect(summaryLooksUnread(summary(ROOT_A, 10), null, 'unread')).toBe(true)
    expect(summaryLooksUnread(summary(ROOT_A, 10), null, 'silent')).toBe(false)
  })

  it('compares against the read position when there is one', () => {
    expect(summaryLooksUnread(summary(ROOT_A, 10), 9, 'unread')).toBe(true)
    expect(summaryLooksUnread(summary(ROOT_A, 10), 10, 'unread')).toBe(false)
  })
})

describe('folded redactions (#1273)', () => {
  /** `$gone` was displayed redacted; its redaction arrived at `redactionAt`. */
  const gone = (arrival: number, redactionAt: number): ReceiptCandidate => ({
    ...ev('$gone', arrival),
    redaction_event_id: '$redaction',
    redaction_arrival_order: redactionAt,
  })

  it('reports each folded redaction in its own arrival position', () => {
    expect(foldedRedactions([ev('$kept', 100), gone(101, 110)])).toEqual([
      ev('$redaction', 110),
    ])
  })

  it('reports nothing without both redaction fields', () => {
    // An event cached before the server reported the arrival order.
    const cached = { ...ev('$gone', 101), redaction_event_id: '$redaction' }
    expect(foldedRedactions([cached])).toEqual([])
  })

  it('lets the room target reach a trailing redaction once caught up', () => {
    const displayed = [ev('$main', 100), gone(101, 110)]
    const { target, blocker } = computeRoomReceipt(
      caughtUp({ displayed, loaded: displayed, threads: [] }),
    )
    expect(target).toEqual(ev('$redaction', 110))
    expect(blocker).toBeNull()
  })

  // The review case: the redaction arrived after an unread reply, so naming it
  // would acknowledge the reply. The base stays on the real displayed event.
  it('does not lift the base over an unread thread reply below the redaction', () => {
    const displayed = [ev('$main', 100), gone(101, 110)]
    const { target, blocker } = computeRoomReceipt(
      caughtUp({
        displayed,
        loaded: [...displayed, ev('$a1', 105, ROOT_A)],
        // A summary with no marker of its own reads as unread.
        threads: [{ summary: summary(ROOT_A, 50), marker: null }],
      }),
    )
    expect(target?.event_id).toBe('$gone')
    expect(blocker).toBe(102)
  })

  it('stops at an unread reply no summary knows about yet', () => {
    const displayed = [ev('$main', 100), gone(101, 110)]
    const { target, blocker } = computeRoomReceipt(
      caughtUp({
        displayed,
        loaded: [...displayed, ev('$b1', 105, ROOT_B)],
        threads: [],
      }),
    )
    expect(target?.event_id).toBe('$gone')
    expect(blocker).toBe(105)
  })

  it('names a redaction that sits below the first unread reply', () => {
    const displayed = [ev('$main', 100), gone(101, 104)]
    const { target, blocker } = computeRoomReceipt(
      caughtUp({
        displayed,
        loaded: [...displayed, ev('$b1', 105, ROOT_B)],
        threads: [],
      }),
    )
    expect(target).toEqual(ev('$redaction', 104))
    expect(blocker).toBe(105)
  })

  it('keeps the base over a redaction that arrived below it', () => {
    // `$old` was redacted before `$main` arrived. Its redaction is no
    // extension, and the final merge must not prefer it over the base.
    const displayed = [{ ...gone(90, 95), event_id: '$old' }, ev('$main', 100)]
    const { target, blocker } = computeRoomReceipt(
      caughtUp({ displayed, loaded: displayed, threads: [] }),
    )
    expect(target).toEqual(ev('$main', 100))
    expect(blocker).toBeNull()
  })

  it('does not name a redaction at the blocker itself', () => {
    // Two real events never share an arrival order. The tie pins the bound
    // as strict, the same `maxByArrivalOrderBelow` the thread extension uses.
    const displayed = [ev('$main', 100), gone(101, 105)]
    const { target, blocker } = computeRoomReceipt(
      caughtUp({
        displayed,
        loaded: [...displayed, ev('$b1', 105, ROOT_B)],
        threads: [],
      }),
    )
    expect(blocker).toBe(105)
    expect(target?.event_id).toBe('$gone')
  })

  it('does not extend while a thread is known unread from live events', () => {
    // `displayed` already lacks the rows the cutoff withheld, such as a main
    // timeline message arriving after it; the redaction must not cover them.
    const displayed = [ev('$main', 100), gone(101, 110)]
    const { target } = computeRoomReceipt(
      caughtUp({
        displayed,
        loaded: displayed,
        threads: [],
        knownUnreadCutoff: 42,
      }),
    )
    expect(target?.event_id).toBe('$gone')
  })

  it('does not extend while the thread stores are still loading', () => {
    const displayed = [ev('$main', 100), gone(101, 110)]
    const { target } = computeRoomReceipt(
      caughtUp({
        displayed,
        loaded: displayed,
        threads: [],
        storesLoaded: false,
      }),
    )
    expect(target?.event_id).toBe('$gone')
  })
})
