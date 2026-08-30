import { expect, test, type Page } from '@playwright/test'
import { ACCOUNT_ID, ROOM_URL, signIn } from './helpers'
import {
  roomOpenRequests,
  roomOpenSummary,
  throttleNetwork,
  unthrottleNetwork,
} from './perf-helpers'

/**
 * Groundwork for "a newly-opened room takes tens of seconds to paint on a weak
 * cell link".
 *
 * `RoomPage`'s mount effect fires four requests at once — the room list, the
 * unpaginated member list, thread summaries, and the timeline page — and only
 * the last of them gates the paint. This lane does not prove which of them is
 * responsible on a real link; a mock on loopback cannot. What it proves is that
 * the readout **discriminates** between them, so a reading taken on a phone can
 * be trusted to mean what it says.
 *
 * The unit tests in `src/perf.test.ts` cover the summary's arithmetic. What
 * only a browser shows is that it emits at all, with real numbers, from real
 * requests — the exact failure `boot-telemetry.spec.ts` exists to prevent for
 * the room-list summary.
 *
 * The device procedure this feeds, and how to read the line it produces, is
 * `docs/web-slow-link-measurement.md`.
 */

const SECOND_ROOM_URL = `/${ACCOUNT_ID}/rooms/${encodeURIComponent('!long:hs')}`
/**
 * The mock's named hold for the room-list GET, worth 1 s.
 *
 * Deliberately shorter than the summary's own three-second grace: the 3 s
 * `held` raced it, and a room list that settles at the same moment the grace
 * expires makes this assertion a coin flip.
 */
const HELD = 'slow'

async function setRoomsHold(page: Page, hold: string): Promise<void> {
  const response = await page.request.post(`/__e2e/rooms-delay?hold=${hold}`)
  expect(response.ok()).toBe(true)
}

// The mock is one process shared by every spec, and a leaked hold looks like a
// hang three specs later — `room-switch-warm.spec.ts` learned this the hard
// way, so reset on failure too, not only on the happy path.
test.afterEach(async ({ page }) => {
  await setRoomsHold(page, 'none')
})

/**
 * Load the app, then throttle — in that order, deliberately. The throttle
 * applies to the whole context, so throttling first would spend the budget on
 * the bundle and measure a download this lane does not care about.
 */
async function coldRoomOpenOnSlowLink(page: Page): Promise<void> {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  // Instrumentation through the stored flag, not `?perf=1`: the URL flag
  // latches inside `perfEnabled()` before any store exists, which is the
  // ordering trap `boot-telemetry.spec.ts` documents.
  await page.addInitScript(() => sessionStorage.setItem('axon.perf', '1'))
  await page.goto('/')
  await expect(page.getByText('E2E Room')).toBeVisible()
}

test('the room-open summary carries real numbers from a real browser', async ({
  page,
}) => {
  await coldRoomOpenOnSlowLink(page)
  const cdp = await throttleNetwork(page, 'slow-3g')
  try {
    await page.goto(SECOND_ROOM_URL)
    await expect(page.getByText('only in the second room')).toBeVisible({
      timeout: 30_000,
    })

    // Painted rows are not the summary: it is emitted two frames after the
    // head fetch settles, and only once the requests beside it have settled
    // or the grace period has expired. Reading once right after the paint
    // caught Chromium in time and lost the race on WebKit and Firefox.
    await expect
      .poll(async () => await roomOpenSummary(page), { timeout: 30_000 })
      .not.toBeNull()

    const summary = await roomOpenSummary(page)
    expect(summary!.phase).toBe('settled')
    // Every field null would be indistinguishable from broken instrumentation,
    // which is the whole reason this runs in a browser rather than jsdom.
    expect(typeof summary!.net).toBe('number')
    expect(typeof summary!.rows).toBe('number')
    expect(summary!.attempts as number).toBeGreaterThanOrEqual(1)

    // The requests it was competing with, named individually.
    const requests = await roomOpenRequests(page)
    expect(requests.length).toBeGreaterThan(0)
    for (const request of requests) {
      expect(typeof request.route).toBe('string')
      expect(typeof request.total).toBe('number')
    }
    // Same-origin under the dev proxy, so the breakdown must be exposed. If
    // this ever fails against a real deployment it is `Timing-Allow-Origin`,
    // not the readout — which is the distinction the `cors` flag records.
    expect(requests.some((request) => request.cors === false)).toBe(true)
  } finally {
    await unthrottleNetwork(cdp)
  }
})

test('the summary separates the timeline page from the room list beside it', async ({
  page,
}) => {
  await coldRoomOpenOnSlowLink(page)
  // Hold the room-list GET. The timeline page does not depend on it, so a
  // readout that cannot tell them apart would report the room open as slow.
  await setRoomsHold(page, HELD)

  await page.goto(SECOND_ROOM_URL)
  await expect(page.getByText('only in the second room')).toBeVisible({
    timeout: 30_000,
  })
  // The room list settles a second or more later; wait for it so `list` is
  // filled in rather than racing the summary.
  await expect
    .poll(async () => (await roomOpenSummary(page))?.list ?? null, {
      timeout: 30_000,
    })
    .not.toBeNull()

  const summary = await roomOpenSummary(page)
  const rows = summary!.rows as number
  const list = summary!.list as number
  // The discrimination this lane exists to prove: messages painted well before
  // the held room list settled, and the readout says so in one line.
  expect(rows).toBeLessThan(list)
  expect(list).toBeGreaterThan(500)
  // And it was actually requested — `list` is only meaningful against the
  // knowledge that a request went out, which is what `pending` disambiguates.
  expect(summary!.pending).toBeNull()
})

test('a warm re-entry is labelled, so it cannot be read as a cold open', async ({
  page,
}) => {
  await coldRoomOpenOnSlowLink(page)
  await page.goto(SECOND_ROOM_URL)
  await expect(page.getByText('only in the second room')).toBeVisible()

  // Leave and return within the session: ADR 0085 phase 1 keeps the store warm,
  // so this paints instantly and measures the gap-fill, not a cold open. A
  // reading that did not say which it was would be worthless on a phone.
  await page.locator(`a[href="${ROOM_URL}"]`).click()
  await expect(page.locator('.media-figure').first()).toBeVisible()
  await page.locator(`a[href="${SECOND_ROOM_URL}"]`).click()
  await expect(page.getByText('only in the second room')).toBeVisible()

  await expect
    .poll(async () => (await roomOpenSummary(page))?.warm ?? null)
    .toBe(true)
})
