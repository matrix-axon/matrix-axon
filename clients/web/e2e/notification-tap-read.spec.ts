import { expect, test, type Page } from '@playwright/test'
import { ACCOUNT_ID, ROOM_ID, signIn } from './helpers'

/**
 * A notification tap opens the room through `?event=<the notified message>`
 * (`notificationHref`), which is usually the room's newest message. The room
 * must be marked read once that view has shown its newest row — not only after
 * the user switches away and back.
 *
 * This needs a real browser: whether the newest row's bottom is on screen is a
 * question of layout and `IntersectionObserver`, neither of which jsdom has.
 */

const COUNT = 40

const message = (n: number) => ({
  account_id: ACCOUNT_ID,
  event_id: `$m${n}`,
  room_id: ROOM_ID,
  sender: '@alice:hs',
  origin_ts: 1_000_000 + n * 1_000,
  arrival_order: n,
  type: 'm.room.message',
  body: `message number ${n}`,
  content: { msgtype: 'm.text', body: `message number ${n}` },
  redacted: false,
  edited: false,
  edit_count: 0,
  state_key: null,
  reactions: null,
  relates_to: null,
})

const EVENTS = Array.from({ length: COUNT }, (_, i) => message(i + 1))
const NEWEST = EVENTS[COUNT - 1]

/** A thread on the newest message, with enough replies to overflow the panel. */
const REPLIES = Array.from({ length: 30 }, (_, i) => ({
  ...message(COUNT + 1 + i),
  event_id: `$r${i + 1}`,
  body: `reply number ${i + 1}`,
  content: { msgtype: 'm.text', body: `reply number ${i + 1}` },
  relates_to: { rel_type: 'm.thread', event_id: NEWEST.event_id },
}))
const NEWEST_REPLY = REPLIES[REPLIES.length - 1]
const ALL = [...EVENTS, ...REPLIES]

/**
 * Open the room at `query`, recording every outbound receipt. Thread members
 * are in the room's own timeline (hidden there), as they are from the server.
 */
async function openAt(page: Page, query: string): Promise<string[]> {
  await signIn(page)
  // A phone, so forty messages overflow the pane by a wide margin.
  await page.setViewportSize({ width: 390, height: 844 })
  await page.route(/\/rooms\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({
      json: {
        data: { events: [...ALL].reverse(), next_cursor: null },
      },
    }),
  )
  await page.route(/\/threads\/[^/]+\/timeline(?:\?.*)?$/, (route) =>
    route.fulfill({
      json: {
        data: { events: [...REPLIES].reverse(), next_cursor: null },
      },
    }),
  )
  await page.route(/\/events\/[^/?]+$/, (route) => {
    const id = decodeURIComponent(
      new URL(route.request().url()).pathname.split('/').pop() ?? '',
    )
    const found = ALL.find((event) => event.event_id === id)
    return found === undefined
      ? route.fulfill({ status: 404, json: {} })
      : route.fulfill({ json: { data: found } })
  })
  const receipts: string[] = []
  page.on('request', (request) => {
    if (
      request.method() === 'POST' &&
      /\/rooms\/[^/]+\/read$/.test(new URL(request.url()).pathname)
    ) {
      const body = request.postDataJSON() as { event_id?: string } | null
      if (typeof body?.event_id === 'string') {
        receipts.push(body.event_id)
      }
    }
  })
  await page.goto(`/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}${query}`)
  return receipts
}

/** Open the room anchored on main-timeline message `anchor`. */
async function openAnchored(page: Page, anchor: number): Promise<string[]> {
  const receipts = await openAt(
    page,
    `?event=${encodeURIComponent(`$m${anchor}`)}`,
  )
  await expect(
    page.locator('.room-stream').getByText(`message number ${anchor}`, {
      exact: true,
    }),
  ).toBeVisible()
  return receipts
}

test('a tap on the newest message marks the room read', async ({ page }) => {
  const receipts = await openAnchored(page, COUNT)

  await expect.poll(() => receipts).toContain(NEWEST.event_id)
  // The highlight is what the tap was for; the anchor stays in the URL.
  await expect(page).toHaveURL(/\?event=%24m40$/)
})

test('an anchor in history claims nothing until the newest row is seen', async ({
  page,
}) => {
  const receipts = await openAnchored(page, 5)

  // Past the receipt debounce: nothing sent means nothing was going to be.
  await page.waitForTimeout(2_000)
  expect(receipts).toEqual([])

  // Reading down to the newest message is what an ordinary open shows, so it
  // may claim the same.
  await page
    .locator('.room-stream > .timeline')
    .evaluate((el) => el.scrollTo({ top: el.scrollHeight }))
  await expect.poll(() => receipts).toContain(NEWEST.event_id)
})

test('a tap on the newest thread reply marks the thread read', async ({
  page,
}) => {
  // `notificationHref` for a thread reply: the thread stays open on the reply.
  const receipts = await openAt(
    page,
    `?thread=${encodeURIComponent(NEWEST.event_id)}&event=${encodeURIComponent(NEWEST_REPLY.event_id)}`,
  )
  await expect(
    page.locator('.thread-panel').getByText('reply number 30', { exact: true }),
  ).toBeVisible()

  // The panel jumps to the reply, walks forward to learn it is the live end,
  // and — with the reply on screen — names it.
  await expect.poll(() => receipts).toContain(NEWEST_REPLY.event_id)
  await expect(page).toHaveURL(/&event=%24r30$/)
})
