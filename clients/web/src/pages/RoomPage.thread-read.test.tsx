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
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import { RECEIPT_DEBOUNCE_MS } from '../stores/ephemeral-sender'
import type { EventDto } from '../stores/timeline'
import { TEST_BASE_URL, testServices } from '../test/services'
import { RoomPage } from './RoomPage'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const ROOT = '$root'
const OTHER_ROOT = '$other-root'
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
// A second thread, used only by the "another thread is unread" case.
const OTHER_ROOT_EVENT = event(OTHER_ROOT, T0 - 9 * MINUTE, 2510)
const OTHER_REPLY = event('$other-reply', T0 - 3 * MINUTE, 2580, OTHER_ROOT)

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
    /** Hang the thread-summary read, so the panel's gate sees "not loaded". */
    threadsPending?: boolean
    /** Hang device-state hydration, so read markers never arrive. */
    deviceStatePending?: boolean
    /** Replies the thread endpoint serves, newest first. */
    replies?: EventDto[]
  } = {},
) {
  const roomEvents = [
    REPLY_2,
    REPLY_1,
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
          },
        ],
      }),
    ),
    http.get(TIMELINE_PATH, () =>
      HttpResponse.json({ data: { events: roomEvents, next_cursor: null } }),
    ),
    http.get(threadTimelinePath(ROOT), () =>
      HttpResponse.json({
        data: {
          events: options.replies ?? [REPLY_2, REPLY_1],
          next_cursor: null,
        },
      }),
    ),
    http.get(threadTimelinePath(OTHER_ROOT), () =>
      HttpResponse.json({ data: { events: [OTHER_REPLY], next_cursor: null } }),
    ),
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`,
      () =>
        options.threadsPending === true
          ? new Promise<Response>(() => {})
          : HttpResponse.json({ data: options.threads ?? [MAIN_THREAD] }),
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
    http.get(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () =>
      options.deviceStatePending === true
        ? new Promise<Response>(() => {})
        : HttpResponse.json({
            data: { namespace: 'read_markers', entries: {} },
          }),
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
  const services = testServices()
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

  it('sends nothing while another thread in the room is unread', async () => {
    const { reads, findByText } = renderRoom({
      query: `?thread=${encodeURIComponent(ROOT)}`,
      threads: [MAIN_THREAD, OTHER_THREAD],
    })
    await findByText(`body of ${REPLY_2.event_id}`)
    await settleReceipts()

    // `$other-reply` sits between the room's receipt floor and `$reply2` in
    // arrival order, and the user has not opened its thread. An unthreaded
    // receipt naming `$reply2` would acknowledge it anyway.
    expect(reads).not.toContain(REPLY_2.event_id)
  })

  it('sends nothing before the thread summaries have loaded', async () => {
    const { reads, findByText } = renderRoom({
      query: `?thread=${encodeURIComponent(ROOT)}`,
      threadsPending: true,
    })
    await findByText(`body of ${REPLY_2.event_id}`)
    await settleReceipts()

    // An empty summary map is "not fetched yet" as often as "no threads".
    // Reading the second from the first opens the gate over threads nobody has
    // looked at.
    expect(reads).not.toContain(REPLY_2.event_id)
  })

  it('sends nothing before read markers have hydrated', async () => {
    const { reads, findByText } = renderRoom({
      query: `?thread=${encodeURIComponent(ROOT)}`,
      deviceStatePending: true,
    })
    await findByText(`body of ${REPLY_2.event_id}`)
    await settleReceipts()

    // Without markers, `reconcileSummary` records nothing at all, so every
    // thread in the room reads as read — the same absence-of-evidence trap.
    expect(reads).not.toContain(REPLY_2.event_id)
  })
})
