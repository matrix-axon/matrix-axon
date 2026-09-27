import { expect, test, type Page, type Request } from '@playwright/test'
import { ACCOUNT_ID, openRoom, ROOM_URL } from './helpers'

/**
 * ADR 0085 phase 1: a room re-entered within one session paints its loaded
 * timeline immediately and reconciles it in place, instead of blanking to
 * "Loading messages…" until the network answers.
 *
 * jsdom already covers the merge; what only a browser can show is that the
 * paint happens *before* the response — so the mock is told to hold the
 * timeline GET open (`/__e2e/timeline-delay`), and the assertions run inside
 * that window. The cold-reload control at the end is what keeps this from
 * passing vacuously: if the delay were not in force, a blank first paint would
 * be indistinguishable from a warm one.
 */

const SECOND_ROOM_URL = `/${ACCOUNT_ID}/rooms/${encodeURIComponent('!long:hs')}`
/** Only this room's history holds it, so seeing it proves whose slice painted. */
const SECOND_ROOM_MESSAGE = 'only in the second room'
/** The mock's named hold, worth 3 s — long enough to assert inside. */
const HELD = 'held'
/** Short enough that anything it catches cannot have waited on the hold. */
const BEFORE_RESPONSE = { timeout: 1000 }

/**
 * The second room's own timeline GET. Armed before the reload, since a wait
 * armed after it could miss a request that fires in the same batch as the
 * commit. The outgoing document cannot satisfy it either: its own GET for this
 * room went out on the re-entry above, before this wait existed, and the
 * reload tears the document down.
 */
function isSecondRoomTimeline(request: Request): boolean {
  return decodeURIComponent(new URL(request.url()).pathname).endsWith(
    '/rooms/!long:hs/timeline',
  )
}

async function setTimelineHold(page: Page, hold: string): Promise<void> {
  const response = await page.request.post(`/__e2e/timeline-delay?hold=${hold}`)
  expect(response.ok()).toBe(true)
}

// The mock is one process shared by every spec, so the hold must not outlive
// this file — a stray delay would look like a hang three specs later.
test.afterEach(async ({ page }) => {
  await setTimelineHold(page, 'none')
})

test('a re-entered room paints its timeline before the refetch settles', async ({
  page,
}) => {
  await openRoom(page)
  await page.locator(`a[href="${SECOND_ROOM_URL}"]`).click()
  await expect(page.getByText(SECOND_ROOM_MESSAGE)).toBeVisible()

  // Leave it. Waiting on the other room's own content proves the switch
  // completed, so nothing from this room is still in flight.
  await page.locator(`a[href="${ROOM_URL}"]`).click()
  await expect(page.locator('.media-figure').first()).toBeVisible()

  await setTimelineHold(page, HELD)
  await page.locator(`a[href="${SECOND_ROOM_URL}"]`).click()

  // The warm store, painted while the gap-fill request is still open.
  await expect(page.getByText(SECOND_ROOM_MESSAGE)).toBeVisible(BEFORE_RESPONSE)
  await expect(page.getByText('Loading messages…')).toBeHidden(BEFORE_RESPONSE)

  // Control: a full document load throws the store away, so the same held
  // request *does* blank the timeline. Without this the assertions above would
  // also pass against a mock that answered instantly.
  //
  // Keyed on the held request, not on `reload()`. That waits for `load`, and
  // the app issues this GET before `load` fires, so a `load` that is slow in CI
  // can land after the 3 s hold has already released: the placeholder has come
  // and gone before a post-`reload()` assertion runs (#391). The request being
  // issued is the moment the hold starts, whenever `load` happens.
  //
  // Seeing the placeholder is not enough on its own: with no hold at all the
  // request still blanks the room for a few milliseconds, and WebKit catches
  // that flash more often than not. So the answer must also still be
  // outstanding once the placeholder has been seen — which only the hold can
  // make true.
  const request = page.waitForRequest(isSecondRoomTimeline)
  await page.reload({ waitUntil: 'commit' })
  const held = await request
  await expect(page.getByText('Loading messages…')).toBeVisible(BEFORE_RESPONSE)
  const seenAt = Date.now()
  await held.response()
  expect(Date.now() - seenAt).toBeGreaterThan(BEFORE_RESPONSE.timeout)
  await expect(page.getByText(SECOND_ROOM_MESSAGE)).toBeVisible()
})
