import { expect, test, type Page } from '@playwright/test'
import { ROOM_URL, signIn } from './helpers'

/**
 * A room whose first page starts arriving and then stops must fail at the
 * request deadline, not sit on "Loading messages…" until the app is killed.
 *
 * Each engine fails it differently without `fetchWithinDeadline`
 * (`src/api/client.ts`), which is why it runs in real engines:
 *
 * - WebKit aborts a request still waiting for headers, but not a body that
 *   stalled after them when the signal came on a `Request` object. The room
 *   stays on "Loading messages…" for as long as the spec waits. On an iPhone
 *   that was 97 s, for a timeline whose headers had arrived in 640 ms.
 *   A JS race against `request.signal` was not enough either: WebKit can
 *   garbage-collect the signal chain behind it, and this spec failed that
 *   version too. The race has to be against the deadline itself.
 * - Chromium does abort the body, but openapi-fetch reads the body outside
 *   the try that runs `onError`. The failure therefore reached the banner
 *   unreworded, as "The user aborted a request.".
 *
 * It takes the full production deadline (`API_REQUEST_TIMEOUT_MS`, 20 s)
 * because nothing shortens that in a built bundle, and the e2e lane
 * deliberately tests the bundle that ships.
 */

const DEADLINE_AND_SLACK = { timeout: 35_000 }

async function setStall(page: Page, enabled: boolean): Promise<void> {
  const response = await page.request.post(
    `/__e2e/timeline-stall?enabled=${enabled}`,
  )
  expect(response.ok()).toBe(true)
}

// The mock is shared by every spec in the run, and a stall left on would hang
// the next spec that opens this room, so it is reset on failure too.
test.afterEach(async ({ page }) => {
  await setStall(page, false)
})

test('a timeline body that stalls mid-transfer fails at the deadline', async ({
  page,
}) => {
  test.setTimeout(60_000)
  await signIn(page)
  await setStall(page, true)

  await page.goto(ROOM_URL)
  await expect(page.getByText('Loading messages…')).toBeVisible()

  await expect(
    page.getByText('Axon did not respond in time', { exact: false }),
  ).toBeVisible(DEADLINE_AND_SLACK)
  await expect(page.getByText('Loading messages…')).toHaveCount(0)
})
