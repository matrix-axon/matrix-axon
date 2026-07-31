import { expect, test, type Page } from '@playwright/test'
import { signIn } from './helpers'

/**
 * ADR 0085 phase 2: a returning tab paints its room list from IndexedDB
 * instead of waiting out `/v1/rooms` — the 1,298 ms of server think-time the
 * ADR measured on a production account.
 *
 * jsdom covers the store and the merge; what only a browser can show is the
 * cache surviving a **document load** — a real IndexedDB, a real reload, and
 * the paint happening before the response. So the mock is told to hold the
 * room-list GET open (`/__e2e/rooms-delay`) and the assertions run inside that
 * window. The cold-load control is what stops this passing vacuously: with the
 * cache disabled, the very same hold must blank the list.
 */

/** The mock's named hold, worth 3 s — long enough to assert inside. */
const HELD = 'held'
/** Short enough that anything it catches cannot have waited on the hold. */
const BEFORE_RESPONSE = { timeout: 1000 }

async function setRoomsHold(page: Page, hold: string): Promise<void> {
  const response = await page.request.post(`/__e2e/rooms-delay?hold=${hold}`)
  expect(response.ok()).toBe(true)
}

// The mock is one process shared by every spec, so neither the hold nor the
// failure may outlive this file — a stray one would look like a hang, or a
// flake, three specs later.
test.afterEach(async ({ page }) => {
  await setRoomsHold(page, 'none')
  await page.request.post('/__e2e/rooms-fail?fail=false')
})

test('a returning tab paints its room list before the request settles', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })

  // First load fills the cache. Waiting on a row proves the refresh settled,
  // which is what triggers the write-back.
  await page.goto('/')
  await expect(page.getByText('E2E Room')).toBeVisible()

  await setRoomsHold(page, HELD)
  await page.reload()

  // The whole claim of this phase, in one assertion: rows on screen while the
  // room-list request is still open.
  await expect(page.getByText('E2E Room')).toBeVisible(BEFORE_RESPONSE)
  await expect(page.getByText('Loading rooms…')).toBeHidden(BEFORE_RESPONSE)
  // And honest about it.
  await expect(page.getByText('Updating…')).toBeVisible(BEFORE_RESPONSE)

  // The affordance goes when the answer lands, leaving an ordinary list.
  await expect(page.getByText('Updating…')).toBeHidden({ timeout: 10_000 })
  await expect(page.getByText('E2E Room')).toBeVisible()
})

test('the same hold blanks the list once the cache is turned off', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto('/')
  await expect(page.getByText('E2E Room')).toBeVisible()

  await page.goto('/settings')
  const toggle = page.getByLabel('Keep the room list on this device')
  await expect(toggle).toBeChecked()
  await toggle.uncheck()

  await setRoomsHold(page, HELD)
  await page.goto('/')

  // Turning the setting off must erase what was already stored, not merely
  // stop adding to it — so this is the pre-phase-2 behavior, exactly.
  await expect(page.getByText('Loading rooms…')).toBeVisible(BEFORE_RESPONSE)
  await expect(page.getByText('E2E Room')).toBeVisible({ timeout: 10_000 })
})

test('cached rooms survive a refresh that never lands', async ({ page }) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto('/')
  await expect(page.getByText('E2E Room')).toBeVisible()

  // The only "offline" a client with no service worker can be tested against:
  // the bundle is already loaded, the API is not reachable. A radios-off reload
  // never reaches the document at all and so tests nothing about the cache.
  expect((await page.request.post('/__e2e/rooms-fail?fail=true')).ok()).toBe(
    true,
  )
  // The failure must arrive *after* the restore, or this proves nothing: an
  // instant failure lands before the cache read resolves, the restore then
  // paints over it, and a store that wiped its rows on error would still pass.
  await setRoomsHold(page, HELD)
  await page.reload()

  // Restored first, and still there once the refresh has failed.
  await expect(page.getByText('E2E Room')).toBeVisible(BEFORE_RESPONSE)
  await expect(page.getByRole('alert')).toBeVisible({ timeout: 10_000 })
  await expect(page.getByText('E2E Room')).toBeVisible()
  await expect(page.getByText('Updating…')).toBeVisible()
  await expect(page.getByRole('alert')).toContainText('room list unavailable')
  await expect(page.getByText('Loading rooms…')).toBeHidden()

  // And recovers in place once the server comes back, with no reload.
  expect((await page.request.post('/__e2e/rooms-fail?fail=false')).ok()).toBe(
    true,
  )
  await setRoomsHold(page, 'none')
  await page.getByRole('button', { name: 'Dismiss' }).click()
  await page.reload()
  await expect(page.getByText('Updating…')).toBeHidden({ timeout: 10_000 })
  await expect(page.getByText('E2E Room')).toBeVisible()
})
