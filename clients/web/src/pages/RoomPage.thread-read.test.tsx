/**
 * ADR 0096 — a thread view may name the room's Matrix read receipt.
 *
 * The room in #207, with its real shape: the arrival-newest events are all
 * thread replies, and the newest event the main timeline renders sits below
 * them. Nothing the room view can name covers the replies, so the receipt stops
 * short, matrix-sdk keeps counting them, and the badge returns on every load.
 *
 * The fix widens the candidate set to the members of a thread the panel has
 * displayed, behind a gate — so these tests are as much about the cases that
 * must send *nothing* as the one that must send.
 */
import { cleanup, fireEvent, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { LocationProvider, Route, Router } from 'preact-iso'
import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  it,
  vi,
} from 'vitest'
import { ServicesContext } from '../services'
import {
  READ_MARKERS_NAMESPACE,
  THREAD_READ_MARKERS_NAMESPACE,
} from '../stores/device-state'
import { RECEIPT_DEBOUNCE_MS } from '../stores/ephemeral-sender'
import type { EventDto } from '../stores/timeline'
import { TEST_BASE_URL, testServices } from '../test/services'
import { RoomPage } from './RoomPage'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const ROOT = '$root'
const OTHER_ROOT = '$other-root'
const SECOND = 1_000
const MINUTE = 60_000
const T0 = Date.UTC(2026, 7, 19, 12, 0, 0)

const TIMELINE_PATH = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/timeline`
const threadTimelinePath = (root: string) =>
  `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/threads/${encodeURIComponent(root)}/timeline`

/** `relates_to` is free-form in the generated contract, so it needs a cast. */
function event(
  id: string,
  ts: number,
  arrivalOrder: number,
  threadRoot?: string,
): EventDto {
  return {
    account_id: ACCOUNT,
    event_id: id,
    room_id: ROOM,
    sender: '@alice:hs',
    origin_ts: ts,
    arrival_order: arrivalOrder,
    type: 'm.room.message',
    body: `body of ${id}`,
    content: { msgtype: 'm.text', body: `body of ${id}` },
    relates_to:
      threadRoot === undefined
        ? null
        : { rel_type: 'm.thread', event_id: threadRoot },
    redacted: false,
    edited: false,
    edit_count: 0,
    state_key: null,
    reactions: null,
  } as unknown as EventDto
}

// The #207 room. `$main` is the newest event the main timeline renders; every
// arrival-newer event is a thread member, hidden behind its root's badge.
const ROOT_EVENT = event(ROOT, T0 - 10 * MINUTE, 2500)
const MAIN = event('$main', T0 - 2 * MINUTE, 2585)
const REPLY_1 = event('$reply1', T0 - MINUTE, 2598, ROOT)
const REPLY_2 = event('$reply2', T0, 2599, ROOT)
// A second thread. Its replies land on either side of `$main`, which is the
// room view's own receipt target — the side they land on is the whole question.
// `$other-old` is already covered by the room's receipt; `$other-new` is inside
// the window a thread receipt would extend over.
// A backfill *into the open thread*: displayed by the panel, but display-early
// and arrival-late, so the display-last event is not the arrival-max one.
const THREAD_BACKFILL = event('$thread-backfill', T0 - 6 * MINUTE, 2610, ROOT)
/** The open thread's arrival-max reply, redacted. */
const THREAD_REDACTED = {
  ...event('$thread-redacted', T0 + MINUTE, 2612, ROOT),
  redacted: true,
}
const OTHER_ROOT_EVENT = event(OTHER_ROOT, T0 - 9 * MINUTE, 2510)
const OTHER_REPLY = event('$other-reply', T0 - 3 * MINUTE, 2580, OTHER_ROOT)
// A bridge backfill into the *other* thread: stamped before that thread was last
// read, but ingested after everything else in the room.
const BACKFILLED_REPLY = event('$backfilled', T0 - 8 * MINUTE, 2605, OTHER_ROOT)
// The other thread's arrival-max reply, redacted — rendered by nothing when the
// hide-redacted setting is on.
const REDACTED_REPLY = {
  ...event('$redacted', T0 - 30 * 1000, 2604, OTHER_ROOT),
  redacted: true,
}
const OTHER_REPLY_NEW = event(
  '$other-reply-new',
  T0 - 90 * SECOND,
  2590,
  OTHER_ROOT,
)

interface ThreadSummary {
  root_event_id: string
  reply_count: number
  latest_reply_event_id: string
  latest_reply_ts: number
}

const MAIN_THREAD: ThreadSummary = {
  root_event_id: ROOT,
  reply_count: 2,
  latest_reply_event_id: REPLY_2.event_id,
  latest_reply_ts: REPLY_2.origin_ts,
}
const OTHER_THREAD: ThreadSummary = {
  root_event_id: OTHER_ROOT,
  reply_count: 1,
  latest_reply_event_id: OTHER_REPLY.event_id,
  latest_reply_ts: OTHER_REPLY.origin_ts,
}
/** A thread whose newest reply is *not* in the loaded slice. */
const GHOST_THREAD: ThreadSummary = {
  root_event_id: '$ghost-root',
  reply_count: 1,
  latest_reply_event_id: '$ghost-reply',
  latest_reply_ts: T0 + MINUTE,
}
const OTHER_THREAD_NEW: ThreadSummary = {
  ...OTHER_THREAD,
  reply_count: 2,
  latest_reply_event_id: OTHER_REPLY_NEW.event_id,
  latest_reply_ts: OTHER_REPLY_NEW.origin_ts,
}

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  window.history.replaceState(null, '', '/')
})
afterAll(() => server.close())

function renderRoom(
  options: {
    /** `?thread=` / `?event=` on the room URL. */
    query?: string
    threads?: ThreadSummary[]
    /** Hang the room timeline read, so the room's slice never arrives. */
    timelinePending?: boolean
    /** Replies the thread endpoint serves, newest first. */
    replies?: EventDto[]
    /** Add a second thread's reply *above* the room view's own target. */
    foreignReplyInWindow?: boolean
    /** A `read_markers` entry already stored for this room, as a real account
     *  has after any earlier session. */
    storedRoomMarker?: { event_id: string; origin_ts: number }
    /** `thread_read_markers` entries already stored, as reading a thread leaves. */
    storedThreadMarkers?: { root: string; eventId: string; originTs: number }[]
    /** The same, but delivered through hydration and *after* the summaries —
     *  the live ordering on a fresh load. */
    hydratedThreadMarkers?: {
      root: string
      eventId: string
      originTs: number
    }[]
    /** Make the timeline answer *after* the room list, as a warm cache does. */
    timelineSlow?: boolean
    /** Server-derived unread count on the room-list row. */
    notificationCount?: number
    /** Install a spy on `noteUnreadCounts` before mounting. */
    spyRooms?: boolean
    /** Fail the thread-summary fetch, as a transient 5xx does. */
    threadsFail?: boolean
    /** Answer the thread-summary fetch after the receipt debounce. */
    threadsSlow?: boolean
    /** A backfilled reply: old `origin_ts`, high `arrival_order`. */
    backfilledForeignReply?: boolean
    /** Give the read foreign thread a *redacted* arrival-max reply. */
    redactedForeignReply?: boolean
    /** Turn on the hide-redacted-events setting (off by default). */
    hideRedacted?: boolean
    /** Serve the open thread a reply that is display-early and arrival-late. */
    threadHasBackfill?: boolean
    /** Serve the open thread a redacted arrival-max reply. */
    threadHasRedacted?: boolean
    /** Fail device-state hydration, as a repeating HTTP error does. */
    deviceStateFail?: boolean
  } = {},
) {
  const roomEvents = [
    ...(options.redactedForeignReply === true ? [REDACTED_REPLY] : []),
    ...(options.backfilledForeignReply === true ? [BACKFILLED_REPLY] : []),
    REPLY_2,
    REPLY_1,
    ...(options.foreignReplyInWindow === true ? [OTHER_REPLY_NEW] : []),
    MAIN,
    OTHER_REPLY,
    OTHER_ROOT_EVENT,
    ROOT_EVENT,
  ]
  const reads: string[] = []
  server.use(
    http.post(
      `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/read`,
      async ({ request }) => {
        const body = (await request.json()) as { event_id: string }
        reads.push(body.event_id)
        return HttpResponse.json({ data: {} })
      },
    ),
    http.put(
      `${TEST_BASE_URL}/v1/accounts/:accountId/rooms/:roomId/typing`,
      () => HttpResponse.json({ data: {} }),
    ),
    http.get(`${TEST_BASE_URL}/v1/invites`, () =>
      HttpResponse.json({ data: [] }),
    ),
    // Faithful to the report: the room summary's newest event *is* the thread
    // reply, since `last_activity_ts` is `MAX(origin_ts)` over every event.
    http.get(`${TEST_BASE_URL}/v1/rooms`, () =>
      HttpResponse.json({
        data: [
          {
            account_id: ACCOUNT,
            account_user_id: '@me:hs',
            room_id: ROOM,
            name: 'Ops',
            topic: null,
            avatar_url: null,
            canonical_alias: null,
            last_activity_ts: REPLY_2.origin_ts,
            last_event_id: REPLY_2.event_id,
            // Without a real count the badge is 0 whatever the page does, and
            // any assertion about clearing it is vacuous.
            notification_count: options.notificationCount ?? 0,
            highlight_count: 0,
          },
        ],
      }),
    ),
    http.get(TIMELINE_PATH, () =>
      options.timelinePending === true
        ? new Promise<Response>(() => {})
        : HttpResponse.json({
            data: { events: roomEvents, next_cursor: null },
          }),
    ),
    http.get(threadTimelinePath(ROOT), () =>
      HttpResponse.json({
        data: {
          events:
            options.replies ??
            (options.threadHasRedacted === true
              ? [REPLY_2, REPLY_1, THREAD_REDACTED]
              : options.threadHasBackfill === true
                ? [REPLY_2, REPLY_1, THREAD_BACKFILL]
                : [REPLY_2, REPLY_1]),
          next_cursor: null,
        },
      }),
    ),
    http.get(threadTimelinePath(OTHER_ROOT), () =>
      HttpResponse.json({ data: { events: [OTHER_REPLY], next_cursor: null } }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`,
      async () => {
        if (options.threadsFail === true) {
          return new HttpResponse(null, { status: 502 })
        }
        if (options.threadsSlow === true) {
          await new Promise((resolve) =>
            setTimeout(resolve, RECEIPT_DEBOUNCE_MS + 700),
          )
        }
        return HttpResponse.json({ data: options.threads ?? [MAIN_THREAD] })
      },
    ),
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/members`,
      () => HttpResponse.json({ data: [] }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/events/:eventId`,
      ({ params }) => {
        const found = roomEvents.find((e) => e.event_id === params.eventId)
        return found === undefined
          ? new HttpResponse(null, { status: 404 })
          : HttpResponse.json({ data: found })
      },
    ),
    http.get(
      `${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`,
      async ({ params }) => {
        if (options.deviceStateFail === true) {
          return new HttpResponse(null, { status: 500 })
        }
        if (
          params.namespace === 'thread_read_markers' &&
          options.hydratedThreadMarkers !== undefined
        ) {
          // Device state answers after the thread summaries, as it does live.
          await new Promise((resolve) => setTimeout(resolve, 80))
          return HttpResponse.json({
            data: {
              namespace: 'thread_read_markers',
              entries: Object.fromEntries(
                options.hydratedThreadMarkers.map((m) => [
                  `${encodeURIComponent(ROOM)}:${encodeURIComponent(m.root)}`,
                  {
                    value: {
                      room_id: ROOM,
                      root_event_id: m.root,
                      event_id: m.eventId,
                      origin_ts: m.originTs,
                    },
                  },
                ]),
              ),
            },
          })
        }
        return HttpResponse.json({
          data: {
            namespace: params.namespace,
            entries:
              params.namespace === 'read_markers' &&
              options.storedRoomMarker !== undefined
                ? { [ROOM]: { value: options.storedRoomMarker } }
                : {},
          },
        })
      },
    ),
    http.put(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () =>
      HttpResponse.json({ data: { updated_at: '2026-08-19T12:00:00Z' } }),
    ),
  )
  window.history.replaceState(
    null,
    '',
    `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}${options.query ?? ''}`,
  )
  // Anchor the thread-unread recency window to the fixtures (all stamped within
  // minutes of `T0`), so `reconcileSummary` promotion here does not depend on
  // how far the real calendar has drifted past that date.
  const services = testServices({ now: () => T0 + 5 * MINUTE })
  if (options.hideRedacted === true) {
    services.settings.hideRedactedEvents.value = true
  }
  if (options.spyRooms === true) {
    vi.spyOn(services.rooms, 'noteUnreadCounts')
  }
  if (options.storedRoomMarker !== undefined) {
    // Seeded directly rather than through hydration: a `GET` that lands after
    // the first local write is discarded by `settled()`, so serving it from msw
    // models the timing, not the state. What matters here is only that a marker
    // already exists when the page mounts.
    services.deviceState.set(
      ACCOUNT,
      READ_MARKERS_NAMESPACE,
      ROOM,
      options.storedRoomMarker,
    )
  }
  for (const m of options.storedThreadMarkers ?? []) {
    services.deviceState.advanceThreadReadMarker(
      ACCOUNT,
      ROOM,
      m.root,
      m.eventId,
      m.originTs,
    )
  }
  const utils = render(
    <ServicesContext.Provider value={services}>
      <LocationProvider>
        <Router>
          <Route path="/:accountId/rooms/:roomId" component={RoomPage} />
          <Route default component={RoomPage} />
        </Router>
      </LocationProvider>
    </ServicesContext.Provider>,
  )
  return { services, reads, ...utils }
}

/**
 * Past the sender's own debounce, so "nothing sent" means nothing was going to
 * be sent — not that the timer has yet to fire.
 */
async function settleReceipts(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, RECEIPT_DEBOUNCE_MS + 150))
}

describe('a thread view names the room receipt (ADR 0096)', () => {
  it('reading the thread receipts its arrival-newest reply', async () => {
    const { reads, findByText } = renderRoom({
      query: `?thread=${encodeURIComponent(ROOT)}`,
    })
    await findByText(`body of ${REPLY_2.event_id}`)
    await waitFor(() => expect(reads).toContain(REPLY_2.event_id))
  })

  it('reading only the room stops below the thread', async () => {
    const { reads, findByText } = renderRoom()
    await findByText(`body of ${MAIN.event_id}`)
    await settleReceipts()

    // The room view displayed `$main` and nothing arrival-newer: the replies
    // live behind the root's badge. Naming one from here would acknowledge a
    // reply this view never rendered.
    expect(reads).toContain(MAIN.event_id)
    expect(reads).not.toContain(REPLY_2.event_id)
    expect(reads).not.toContain(REPLY_1.event_id)
  })

  it('sends nothing from a thread panel parked in history (#154)', async () => {
    const { services, reads, findByText } = renderRoom({
      query: `?thread=${encodeURIComponent(ROOT)}&event=${encodeURIComponent(REPLY_1.event_id)}`,
    })
    await findByText(`body of ${REPLY_1.event_id}`)
    await settleReceipts()

    // The panel is anchored on an old reply, so it has displayed neither the
    // thread's newest reply nor the room's. All three claims stay put: the
    // receipt, the cross-device thread marker, and the unread-threads entry.
    expect(reads).not.toContain(REPLY_2.event_id)
    expect(
      services.deviceState.threadReadMarker(ACCOUNT, ROOM, ROOT),
    ).toBeNull()
  })

  it('sends nothing while the room stream itself is parked in history', async () => {
    const { services, reads, findByText, findByRole } = renderRoom()
    await findByText(`body of ${MAIN.event_id}`)

    // A jump to date, which parks the main stream *without* touching the URL —
    // the one case where the room's own gate is not doubled by the panel's, and
    // the reason it is a separate term rather than an implied one.
    const timeline = services.timelines.acquire(ACCOUNT, ROOM)
    await timeline.jumpTo(REPLY_2.origin_ts)
    await waitFor(() => expect(timeline.atEnd.value).toBe(false))

    fireEvent.click(await findByRole('button', { name: /replies/ }))
    await findByText(`body of ${REPLY_2.event_id}`)
    await settleReceipts()

    // The thread is genuinely caught up, but the room stream below it is not:
    // main-timeline events between the parked slice and the live end have never
    // been rendered, and an unthreaded receipt at `$reply2` covers them all.
    expect(reads).not.toContain(REPLY_2.event_id)
  })

  it('stops below a reply from a thread this panel is not showing', async () => {
    const { reads, findByText } = renderRoom({
      query: `?thread=${encodeURIComponent(ROOT)}`,
      threads: [MAIN_THREAD, OTHER_THREAD_NEW],
      foreignReplyInWindow: true,
    })
    await findByText(`body of ${REPLY_2.event_id}`)
    await settleReceipts()

    // `$other-reply-new` (2590) sits above the room's own target (`$main`,
    // 2585) and below this thread's tail (2599). Naming 2599 would acknowledge
    // it, and nothing has displayed it — so the pick stops at 2598's neighbour
    // below the ceiling, `$reply1`, and never reaches `$reply2`.
    expect(reads).not.toContain(REPLY_2.event_id)
    expect(reads).not.toContain(REPLY_1.event_id)
    expect(reads).toContain(MAIN.event_id)
  })

  it('flags the thread as unread instead of seeding the marker from it', async () => {
    const { services, findByText } = renderRoom({ threads: [MAIN_THREAD] })
    await findByText(`body of ${MAIN.event_id}`)

    // The summary's newest event is `$reply2`, a thread member. Seeding the
    // room marker from it makes `reconcileSummary` compare the thread against a
    // position derived from that very reply and call it read — so the room
    // badges with nothing anywhere saying which thread to open (#209).
    await waitFor(() =>
      expect(services.threadUnread.isUnread(ACCOUNT, ROOM, ROOT)).toBe(true),
    )
    expect(services.threadUnread.count.value).toBe(1)
    expect(services.deviceState.readMarker(ACCOUNT, ROOM)).toEqual({
      eventId: MAIN.event_id,
      originTs: MAIN.origin_ts,
    })
  })

  it('does not seed the marker from the summary before the timeline lands', async () => {
    // The live ordering, which a plain delay does not reproduce: the room list
    // is restored from IndexedDB (ADR 0085 phase 2) and is on screen before the
    // first timeline page exists, so the summary effect runs against an *empty*
    // slice — where it cannot see that `last_event_id` is a thread member.
    // Observed on a dev instance as `loadedEvents: 0, timelineLoading: true`
    // with the summary already present.
    const { services, findByText } = renderRoom({
      threads: [MAIN_THREAD],
      timelinePending: true,
    })
    await findByText('Ops')
    await settleReceipts()

    // `advanceReadMarker` is forward-only on `origin_ts`, so seeding it from the
    // reply here is permanent: the timeline effect can never walk it back to a
    // main-timeline position, and the thread reads as read forever.
    expect(services.deviceState.readMarker(ACCOUNT, ROOM)?.eventId).not.toBe(
      REPLY_2.event_id,
    )
  })

  it('flags the thread even when the stored room marker already points at it', async () => {
    // The state a real account is left in by any earlier session: the marker was
    // seeded from the reply before the fix existed, and `advanceReadMarker` is
    // forward-only, so nothing walks it back. Preventing new poisoning does not
    // heal this — and `reconcileSummary` still falls back to it.
    const { services, findByText } = renderRoom({
      threads: [MAIN_THREAD],
      storedRoomMarker: {
        event_id: REPLY_2.event_id,
        origin_ts: REPLY_2.origin_ts,
      },
    })
    await findByText(`body of ${MAIN.event_id}`)
    expect(services.deviceState.readMarker(ACCOUNT, ROOM)?.eventId).toBe(
      REPLY_2.event_id,
    )

    await waitFor(() =>
      expect(services.threadUnread.isUnread(ACCOUNT, ROOM, ROOT)).toBe(true),
    )
  })

  it('sends once every thread in the window has been read', async () => {
    const { reads, findByText } = renderRoom({
      query: `?thread=${encodeURIComponent(ROOT)}`,
      threads: [MAIN_THREAD, OTHER_THREAD_NEW],
      foreignReplyInWindow: true,
      // The other thread has been read — its marker covers its newest reply.
      storedThreadMarkers: [
        {
          root: OTHER_ROOT,
          eventId: OTHER_REPLY_NEW.event_id,
          originTs: OTHER_REPLY_NEW.origin_ts,
        },
      ],
    })
    await findByText(`body of ${REPLY_2.event_id}`)

    // A reply the user *has* read must not hold the room's receipt back
    // forever. Otherwise a room with two interleaved threads can never clear:
    // whichever panel is open, the other thread's replies sit in the window.
    await waitFor(() => expect(reads).toContain(REPLY_2.event_id))
  })

  it('acknowledges the whole room once every thread in it is read', async () => {
    // The live sequence from #207's test room: two threads, read in the wrong
    // order. Opening the newest thread first names nothing (the older thread's
    // reply is still unread and holds the ceiling down); opening the older one
    // then names *its* reply. The newest thread is eligible by then, but its
    // panel is closed and nothing revisits it — so the room stays one event
    // short forever. The room view has to be able to close that gap itself.
    const { reads, findByText } = renderRoom({
      threads: [MAIN_THREAD, OTHER_THREAD_NEW],
      foreignReplyInWindow: true,
      storedThreadMarkers: [
        {
          root: ROOT,
          eventId: REPLY_2.event_id,
          originTs: REPLY_2.origin_ts,
        },
        {
          root: OTHER_ROOT,
          eventId: OTHER_REPLY_NEW.event_id,
          originTs: OTHER_REPLY_NEW.origin_ts,
        },
      ],
    })
    await findByText(`body of ${MAIN.event_id}`)

    // No panel is open. Every thread reply in the room is covered by a marker,
    // so the arrival-max event the client can honestly claim is `$reply2`.
    await waitFor(() => expect(reads).toContain(REPLY_2.event_id))
  })

  it('does not extend over a read reply that sits above an unread one', async () => {
    // `$reply2` (2599) is read; `$other-reply-new` (2590) is not, and sits below
    // it. Naming 2599 would acknowledge 2590 — the exact over-claim the bound
    // exists to prevent, now reachable from the room view rather than a panel.
    const { reads, findByText } = renderRoom({
      threads: [MAIN_THREAD, OTHER_THREAD_NEW],
      foreignReplyInWindow: true,
      storedThreadMarkers: [
        { root: ROOT, eventId: REPLY_2.event_id, originTs: REPLY_2.origin_ts },
      ],
    })
    await findByText(`body of ${MAIN.event_id}`)
    await settleReceipts()

    expect(reads).toContain(MAIN.event_id)
    expect(reads).not.toContain(REPLY_2.event_id)
  })

  it('never judges a thread before its markers have hydrated', async () => {
    const { services, findByText } = renderRoom({
      threads: [MAIN_THREAD],
      hydratedThreadMarkers: [
        { root: ROOT, eventId: REPLY_2.event_id, originTs: REPLY_2.origin_ts },
      ],
    })
    // Asserted on the *call*, not on a peak sampled by a timer. The flash this
    // guards lasts one render, and polling for it made the test a coin flip
    // under parallel load: it failed once in a full run and passed in isolation.
    const judged: boolean[] = []
    const reconcile = services.threadUnread.reconcileSummary.bind(
      services.threadUnread,
    )
    vi.spyOn(services.threadUnread, 'reconcileSummary').mockImplementation(
      (summary, context) => {
        judged.push(
          services.deviceState.hydrated(ACCOUNT, THREAD_READ_MARKERS_NAMESPACE),
        )
        reconcile(summary, context)
      },
    )
    await findByText(`body of ${MAIN.event_id}`)
    await settleReceipts()

    // Every judgement was made with the markers in hand. Judging earlier
    // compares a thread against the *room* marker, which flags every thread
    // whose replies are newer than the main timeline — an unread badge that
    // corrects itself a moment later.
    expect(judged.length).toBeGreaterThan(0)
    expect(judged.every(Boolean)).toBe(true)
    expect(services.threadUnread.count.value).toBe(0)
  })

  it('keeps the room-list badge while a thread in it is still unread', async () => {
    // Spied rather than read back: the optimistic clear races the room-list
    // fetch that repopulates the count from the DTO, so the call is the
    // deterministic signal — the same reason `RoomPage.test.tsx` spies it.
    const cleared = (services: ReturnType<typeof testServices>) =>
      vi
        .spyOn(services.rooms, 'noteUnreadCounts')
        .mock.calls.filter(
          (call) => call[1] === ROOM && call[2] === 0 && call[3] === 0,
        )
    const { services, findByText } = renderRoom({
      threads: [MAIN_THREAD],
      notificationCount: 3,
      spyRooms: true,
    })
    await findByText(`body of ${MAIN.event_id}`)
    await settleReceipts()

    // Opening a room does not read its threads. Clearing here leaves the user
    // looking at a room with no badge and an unread count on the Threads
    // button — two indicators disagreeing about the same room.
    await waitFor(() =>
      expect(services.threadUnread.isUnread(ACCOUNT, ROOM, ROOT)).toBe(true),
    )
    expect(cleared(services)).toHaveLength(0)
  })

  it('clears the room-list badge once every thread in it is read', async () => {
    const { services, findByText } = renderRoom({
      threads: [MAIN_THREAD],
      notificationCount: 3,
      spyRooms: true,
      storedThreadMarkers: [
        { root: ROOT, eventId: REPLY_2.event_id, originTs: REPLY_2.origin_ts },
      ],
    })
    await findByText(`body of ${MAIN.event_id}`)

    await waitFor(() =>
      expect(
        vi
          .spyOn(services.rooms, 'noteUnreadCounts')
          .mock.calls.filter(
            (call) => call[1] === ROOM && call[2] === 0 && call[3] === 0,
          ).length,
      ).toBeGreaterThan(0),
    )
    await settleReceipts()
  })

  it('does not treat a backfilled reply as read on an origin_ts comparison', async () => {
    // The other thread's marker covers its newest reply *by timestamp*, and the
    // backfilled reply is stamped older still — so an `origin_ts` comparison
    // calls it read. Nobody displayed it: it arrived after everything else
    // (arrival 2605), which is the only order a receipt is interpreted in.
    const { reads, findByText } = renderRoom({
      threads: [MAIN_THREAD, OTHER_THREAD],
      backfilledForeignReply: true,
      storedThreadMarkers: [
        { root: ROOT, eventId: REPLY_2.event_id, originTs: REPLY_2.origin_ts },
        {
          root: OTHER_ROOT,
          eventId: OTHER_REPLY.event_id,
          originTs: OTHER_REPLY.origin_ts,
        },
      ],
    })
    await findByText(`body of ${MAIN.event_id}`)
    await settleReceipts()

    // `$reply2` (2599) stays claimable — it sits *below* the backfilled reply in
    // arrival order, so naming it acknowledges nothing unseen.
    expect(reads).not.toContain(BACKFILLED_REPLY.event_id)
  })

  it('still clears the room-list badge when the thread fetch failed', async () => {
    // A transient 5xx retries only when a new thread reply arrives. Waiting for
    // success meant one failure froze the badge for good.
    const { services, findByText } = renderRoom({
      notificationCount: 3,
      spyRooms: true,
      threadsFail: true,
    })
    await findByText(`body of ${MAIN.event_id}`)

    await waitFor(() =>
      expect(
        vi
          .spyOn(services.rooms, 'noteUnreadCounts')
          .mock.calls.filter(
            (call) => call[1] === ROOM && call[2] === 0 && call[3] === 0,
          ).length,
      ).toBeGreaterThan(0),
    )
    await settleReceipts()
  })

  it('does not extend while an unread thread sits outside the loaded slice', async () => {
    // The arrival-order bound can only see replies the client holds. This
    // thread's reply is known from the summary and absent from the slice, so
    // nothing below stops the extension from claiming past it — only the cutoff
    // does. Naming `$reply2` here acknowledges every arrival below it, and the
    // ghost reply's position is unknown.
    const { reads, findByText } = renderRoom({
      threads: [MAIN_THREAD, GHOST_THREAD],
      storedThreadMarkers: [
        { root: ROOT, eventId: REPLY_2.event_id, originTs: REPLY_2.origin_ts },
      ],
    })
    await findByText(`body of ${MAIN.event_id}`)
    await settleReceipts()

    expect(reads).toContain(MAIN.event_id)
    expect(reads).not.toContain(REPLY_2.event_id)
  })

  it('does not extend before the thread summaries have arrived', async () => {
    // The summaries answer after the receipt debounce would have fired. Until
    // they do, this room might hold any number of unread threads, and a receipt
    // sent on that assumption cannot be recalled.
    const { reads, findByText } = renderRoom({
      threads: [MAIN_THREAD],
      threadsSlow: true,
      storedThreadMarkers: [
        { root: ROOT, eventId: REPLY_2.event_id, originTs: REPLY_2.origin_ts },
      ],
    })
    await findByText(`body of ${MAIN.event_id}`)
    await settleReceipts()

    expect(reads).not.toContain(REPLY_2.event_id)
  })

  it('does not name a redacted reply the view never rendered', async () => {
    // `$redacted` (2604) is the other thread's arrival-max reply and is covered
    // by its marker, so it passes the read test purely on position — but with
    // the hide-redacted setting on, nothing rendered it. ADR 0089's contract is
    // that a receipt names something that was shown.
    const { reads, findByText } = renderRoom({
      threads: [MAIN_THREAD, OTHER_THREAD],
      redactedForeignReply: true,
      hideRedacted: true,
      storedThreadMarkers: [
        { root: ROOT, eventId: REPLY_2.event_id, originTs: REPLY_2.origin_ts },
        {
          root: OTHER_ROOT,
          eventId: REDACTED_REPLY.event_id,
          originTs: REDACTED_REPLY.origin_ts,
        },
      ],
    })
    await findByText(`body of ${MAIN.event_id}`)
    await settleReceipts()

    expect(reads).not.toContain(REDACTED_REPLY.event_id)
  })

  it('still clears the room-list badge when device state never hydrates', async () => {
    // A repeating HTTP failure on the device-state GET has no retry until the
    // socket drops. Waiting on hydration froze the badge exactly as waiting on
    // the summary fetch did.
    const { services, findByText } = renderRoom({
      notificationCount: 3,
      spyRooms: true,
      deviceStateFail: true,
    })
    await findByText(`body of ${MAIN.event_id}`)

    await waitFor(() =>
      expect(
        vi
          .spyOn(services.rooms, 'noteUnreadCounts')
          .mock.calls.filter(
            (call) => call[1] === ROOM && call[2] === 0 && call[3] === 0,
          ).length,
      ).toBeGreaterThan(0),
    )
    await settleReceipts()
  })

  it('records how far the panel read in arrival order, not just display order', async () => {
    // The thread holds a backfilled reply: display-early (T0 − 6m) and
    // arrival-late (2610). The marker's display position is still `$reply2`,
    // but what it records having *read through* has to be 2610 — otherwise
    // `RoomPage.isRead` later reads the understatement as "still unread", makes
    // that reply a blocker, and the room receipt can never extend past it.
    const { services, findByText } = renderRoom({
      query: `?thread=${encodeURIComponent(ROOT)}`,
      threadHasBackfill: true,
    })
    await findByText(`body of ${REPLY_2.event_id}`)

    await waitFor(() =>
      expect(
        services.deviceState.threadReadMarker(ACCOUNT, ROOM, ROOT),
      ).toEqual({
        roomId: ROOM,
        rootEventId: ROOT,
        eventId: REPLY_2.event_id,
        originTs: REPLY_2.origin_ts,
        arrivalThrough: THREAD_BACKFILL.arrival_order,
      }),
    )
  })

  it('claims nothing from a redacted reply the panel did not render', async () => {
    const { services, reads, findByText } = renderRoom({
      query: `?thread=${encodeURIComponent(ROOT)}`,
      threadHasRedacted: true,
      hideRedacted: true,
    })
    await findByText(`body of ${REPLY_2.event_id}`)
    await settleReceipts()

    // Neither claim may name it: not the receipt, and not the marker's
    // read-through position, which would tell every other view the panel had
    // displayed something it hid.
    expect(reads).not.toContain(THREAD_REDACTED.event_id)
    await waitFor(() =>
      expect(
        services.deviceState.threadReadMarker(ACCOUNT, ROOM, ROOT)
          ?.arrivalThrough,
      ).toBe(REPLY_2.arrival_order),
    )
  })

  it('still sends when the room has other threads the user has never opened', async () => {
    const { reads, findByText } = renderRoom({
      query: `?thread=${encodeURIComponent(ROOT)}`,
      threads: [MAIN_THREAD, OTHER_THREAD],
    })
    await findByText(`body of ${REPLY_2.event_id}`)

    // The live report this route was rewritten for: a real room is full of
    // threads nobody has opened this session, and their replies are *below* the
    // room's own target — already acknowledged by the receipt the room view
    // sends anyway. Requiring them all to be known-read (the first
    // implementation) made the gate unopenable in exactly the room from #207.
    await waitFor(() => expect(reads).toContain(REPLY_2.event_id))
  })

  it('sends nothing before the room stream itself has loaded', async () => {
    const { reads, findByText } = renderRoom({
      query: `?thread=${encodeURIComponent(ROOT)}`,
      timelinePending: true,
    })
    await findByText(`body of ${REPLY_2.event_id}`)
    await settleReceipts()

    // The panel's own endpoint answered first. The ceiling is derived from the
    // room's slice, so an empty one reports "nothing in the way" rather than
    // "nothing known" — and `atEnd` starts true on a cold store, so the room
    // gate does not catch this by itself.
    expect(reads).not.toContain(REPLY_2.event_id)
  })
})
