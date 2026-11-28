/**
 * Issue #337 — a trailing redacted event must not pin the room's unread count.
 *
 * The shape came off a production instance: a bridge sent a message and
 * redacted it five seconds later. The homeserver still counts it, so the room's
 * `notification_count` stays at 1; and the client could never send a receipt
 * that covered it, because the redacted message leaves the timeline and the
 * bodyless `m.room.redaction` behind it is dropped as an unsupported event.
 * With no candidate above the last plain message, the receipt stopped there and
 * the badge came back on every reload.
 *
 * So these tests are about what the receipt may *name*, not what the page
 * renders — the two deliberately disagree about redaction, and nothing else.
 */
import { cleanup, render, waitFor } from '@testing-library/preact'
import { HttpResponse, http } from 'msw'
import { setupServer } from 'msw/node'
import { LocationProvider, Route, Router } from 'preact-iso'
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest'
import { ServicesContext } from '../services'
import { RECEIPT_DEBOUNCE_MS } from '../stores/ephemeral-sender'
import type { EventDto } from '../stores/timeline'
import { emptyRoomInfoHandler } from '../test/room-details'
import { TEST_BASE_URL, testServices } from '../test/services'
import { RoomPage } from './RoomPage'

const ACCOUNT = '6b53f7f0-0000-4000-8000-000000000001'
const ROOM = '!room:hs'
const MINUTE = 60_000
const T0 = Date.UTC(2026, 8, 3, 16, 0, 0)

const TIMELINE_PATH = `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}/timeline`

function event(id: string, ts: number, arrivalOrder: number): EventDto {
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
    relates_to: null,
    redacted: false,
    edited: false,
    edit_count: 0,
    state_key: null,
    reactions: null,
  } as unknown as EventDto
}

/** The last event covered by the receipt the account already holds. */
const READ = event('$read', T0 - 2 * MINUTE, 2957578)
/**
 * The culprit: a real message, redacted moments later. Its arrival position is
 * above `$read`, so it is what the receipt has to reach to clear the count.
 */
const DELETED = { ...event('$deleted', T0, 2957579), redacted: true }
/**
 * The redaction itself, as a bare row — arrival-newest of all, and rendered by
 * nothing: no body, no media, so `isUnsupportedBodylessEvent` drops it whatever
 * the hide-redacted setting says. As a row of its own it must stay unnameable,
 * or the fix would be "name the newest thing regardless", which is a different
 * and wrong rule. (The server never sends this row; it folds the redaction onto
 * its target instead, which is the shape the #1273 tests below use.)
 */
const REDACTION: EventDto = {
  ...event('$redaction', T0 + 5_000, 2957580),
  type: 'm.room.redaction',
  body: null,
  content: {},
  redacts: DELETED.event_id,
} as unknown as EventDto

const server = setupServer(emptyRoomInfoHandler(TEST_BASE_URL))
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => {
  cleanup()
  server.resetHandlers()
  window.history.replaceState(null, '', '/')
})
afterAll(() => server.close())

function renderRoom(options: { hideRedacted: boolean; events?: EventDto[] }) {
  const roomEvents = options.events ?? [REDACTION, DELETED, READ]
  const newest = roomEvents[0]
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
            last_activity_ts: newest.origin_ts,
            last_event_id: newest.event_id,
            // The stuck count itself: without it any claim about clearing the
            // badge is vacuous.
            notification_count: 1,
            highlight_count: 0,
          },
        ],
      }),
    ),
    http.get(TIMELINE_PATH, () =>
      HttpResponse.json({ data: { events: roomEvents, next_cursor: null } }),
    ),
    // No threads: this bug needs none, which is the point — it is not #207.
    http.get(
      `${TEST_BASE_URL}/v1/accounts/${ACCOUNT}/rooms/:roomId/threads`,
      () => HttpResponse.json({ data: [] }),
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
      HttpResponse.json({
        data: { namespace: 'read_markers', entries: {} },
      }),
    ),
    http.put(`${TEST_BASE_URL}/v1/devices/:deviceId/state/:namespace`, () =>
      HttpResponse.json({ data: { updated_at: '2026-09-03T16:00:00Z' } }),
    ),
  )
  window.history.replaceState(
    null,
    '',
    `/${ACCOUNT}/rooms/${encodeURIComponent(ROOM)}`,
  )
  const services = testServices({ now: () => T0 + 5 * MINUTE })
  services.settings.hideRedactedEvents.value = options.hideRedacted
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

/** Past the sender's debounce, so "nothing sent" means nothing was coming. */
async function settleReceipts(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, RECEIPT_DEBOUNCE_MS + 150))
}

describe('a trailing redacted event does not pin the unread count (#337)', () => {
  it('receipts the redacted event even though it is hidden', async () => {
    const { reads, findByText } = renderRoom({ hideRedacted: true })

    // The room rendered, and the redacted message is genuinely not on screen —
    // otherwise this asserts nothing about *hidden* events.
    await findByText('body of $read')
    expect(document.body.textContent).not.toContain('body of $deleted')

    await waitFor(() => expect(reads.length).toBeGreaterThan(0))
    await settleReceipts()

    // The redacted event, not the last plain message below it: that is the
    // difference between a count that clears and one that never does.
    expect(reads).toContain(DELETED.event_id)
    // And not the bodyless redaction above it, which nothing displayed and
    // which no relaxation of the redaction rule should reach.
    expect(reads).not.toContain(REDACTION.event_id)
  })

  it('still receipts it when redactions are shown, through the ordinary path', async () => {
    const { reads, findByText } = renderRoom({ hideRedacted: false })

    await findByText('body of $read')
    await waitFor(() => expect(reads.length).toBeGreaterThan(0))
    await settleReceipts()

    // The user-level workaround for #337 was to turn this setting off. It has
    // to keep working, and for the boring reason — the event is rendered, so it
    // was always a legal target.
    expect(reads).toContain(DELETED.event_id)
    expect(reads).not.toContain(REDACTION.event_id)
  })
})

/**
 * Issue #1273 — the receipt reached the redacted message, and stopped one event
 * short of the redaction behind it.
 *
 * Off the same production instance as #337, in a room with a per-room push
 * rule set to notify on every event. Such a rule matches redactions too, so the
 * redaction was itself the counted notification. The server strips bare
 * redactions from the timeline and folds each onto its target, so the only way
 * a client can name one is through the target's `redaction_arrival_order`.
 */
describe('a trailing redaction folded onto its target is acknowledged (#1273)', () => {
  /** The server's real shape: no bare redaction row, the redaction folded in. */
  const FOLDED = {
    ...DELETED,
    redaction_event_id: REDACTION.event_id,
    redaction_arrival_order: REDACTION.arrival_order,
  } as EventDto

  for (const hideRedacted of [true, false]) {
    it(`receipts the redaction with hide-redacted ${hideRedacted ? 'on' : 'off'}`, async () => {
      const { reads, findByText } = renderRoom({
        hideRedacted,
        events: [FOLDED, READ],
      })

      await findByText('body of $read')
      await waitFor(() => expect(reads).toContain(REDACTION.event_id))
      await settleReceipts()

      // The redaction is the room's newest event, so it is the last word: a
      // receipt naming only the redacted message leaves the redaction counted.
      expect(reads.at(-1)).toBe(REDACTION.event_id)
    })
  }

  it('does not name the redaction of an event the room view does not display', async () => {
    // A thread reply, redacted. The room view hides thread members, so neither
    // the reply nor the redaction folded onto it is the room's to claim.
    const reply = {
      ...event('$reply', T0, 2957579),
      relates_to: { rel_type: 'm.thread', event_id: '$root' },
      redacted: true,
      redaction_event_id: '$reply-redaction',
      redaction_arrival_order: 2957580,
    } as unknown as EventDto
    const { reads, findByText } = renderRoom({
      hideRedacted: true,
      events: [reply, READ],
    })

    await findByText('body of $read')
    await waitFor(() => expect(reads.length).toBeGreaterThan(0))
    await settleReceipts()

    expect(reads).toContain(READ.event_id)
    expect(reads).not.toContain('$reply-redaction')
    expect(reads).not.toContain(reply.event_id)
  })
})
