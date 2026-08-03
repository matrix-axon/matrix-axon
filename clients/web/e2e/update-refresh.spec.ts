import { expect, test, type Page } from '@playwright/test'
import { ROOM_URL, signIn } from './helpers'

/**
 * Automatic refresh when a new build is deployed (ADR 0087).
 *
 * The unit tests drive the policy through injected seams; what only a real
 * browser can show is the part that matters most — that a page which decides to
 * reload actually reloads, and comes back on the new build rather than looping.
 * `/__e2e/version` stages the deploy without a rebuild.
 */
test.describe.configure({ mode: 'serial' })

declare global {
  interface Window {
    /** Marker stamped into the page; its absence proves a navigation happened. */
    __reloaded?: boolean
  }
}

/** Point the origin's manifest at a build the tab is not running. */
async function stageDeploy(page: Page, version: string): Promise<void> {
  const response = await page.request.post(
    `/__e2e/version?version=${version}&release=9.9.9`,
  )
  expect(response.ok()).toBe(true)
}

/** Put the manifest back to whatever `dist/` really holds. */
async function clearDeploy(page: Page): Promise<void> {
  await page.request.post('/__e2e/version?version=')
}

/**
 * Hide the tab and bring it back after `awayMs` of *page* time. The policy
 * measures absence with `Date.now()`, so faking the clock is what lets the test
 * cross the one-minute "the user was away" threshold without waiting a minute.
 */
async function returnAfterAway(page: Page, awayMs: number): Promise<void> {
  await page.evaluate((ms) => {
    const realNow = Date.now.bind(Date)
    let skew = 0
    Date.now = () => realNow() + skew

    Object.defineProperty(document, 'hidden', {
      configurable: true,
      get: () => hidden,
    })
    let hidden = true
    document.dispatchEvent(new Event('visibilitychange'))

    skew = ms
    hidden = false
    document.dispatchEvent(new Event('visibilitychange'))
  }, awayMs)
}

test.afterEach(async ({ page }) => {
  await clearDeploy(page)
})

test('a tab on the current build neither warns nor reloads', async ({
  page,
}) => {
  await signIn(page)
  await page.goto(ROOM_URL)
  await expect(page.getByRole('status').first()).toHaveText('Live')

  await page.evaluate(() => {
    window.__reloaded = true
  })
  await returnAfterAway(page, 10 * 60_000)

  await expect(
    page.getByText('A new version of Axon is available'),
  ).toHaveCount(0)
  // The marker survives, so the page never navigated.
  expect(await page.evaluate(() => window.__reloaded === true)).toBe(true)
})

test('a new build shows the banner while the user is looking', async ({
  page,
}) => {
  await signIn(page)
  await page.goto(ROOM_URL)
  await expect(page.getByRole('status').first()).toHaveText('Live')

  await stageDeploy(page, 'build-from-e2e')
  // Dropping every socket is what a deploy does to a client; the reconnect is
  // the trigger the app uses to re-check.
  await page.request.post('/__e2e/drop-sockets')

  const banner = page.getByText('A new version of Axon is available')
  await expect(banner).toBeVisible({ timeout: 15_000 })
  await expect(page.getByText('9.9.9+build-from-e2e')).toBeVisible()
})

test('the banner reloads onto the served build when clicked', async ({
  page,
}) => {
  await signIn(page)
  await page.goto(ROOM_URL)
  await expect(page.getByRole('status').first()).toHaveText('Live')

  await stageDeploy(page, 'build-from-e2e')
  await page.request.post('/__e2e/drop-sockets')
  await expect(
    page.getByText('A new version of Axon is available'),
  ).toBeVisible({ timeout: 15_000 })

  // Clear the override first: the reload should land on the real build and the
  // banner should be gone, which is what "the update applied" looks like.
  await clearDeploy(page)
  await page.getByRole('button', { name: 'Reload' }).click()

  await expect(page.getByRole('status').first()).toHaveText('Live')
  await expect(
    page.getByText('A new version of Axon is available'),
  ).toHaveCount(0)
})

test('a backgrounded tab reloads itself on return', async ({ page }) => {
  await signIn(page)
  await page.goto(ROOM_URL)
  await expect(page.getByRole('status').first()).toHaveText('Live')

  await page.evaluate(() => {
    window.__reloaded = true
  })

  await stageDeploy(page, 'build-from-e2e')
  await returnAfterAway(page, 90_000)

  // A navigation clears the page context, so the marker is gone.
  await expect
    .poll(() => page.evaluate(() => window.__reloaded === true), {
      timeout: 15_000,
    })
    .toBe(false)
})

// The loop guard. With the origin permanently claiming a build the tab can
// never become, an unguarded implementation reloads forever.
test('a manifest that never matches reloads exactly once', async ({ page }) => {
  await signIn(page)
  await stageDeploy(page, 'a-build-that-never-ships')

  let navigations = 0
  page.on('framenavigated', (frame) => {
    if (frame === page.mainFrame()) {
      navigations += 1
    }
  })

  await page.goto(ROOM_URL)
  await expect(page.getByRole('status').first()).toHaveText('Live')
  await returnAfterAway(page, 90_000)

  // Give a loop time to show itself: the reload, then a settling window in
  // which a broken guard would fire again on every check.
  await page.waitForTimeout(5000)

  // The initial goto plus at most the one permitted reload.
  expect(navigations).toBeLessThanOrEqual(2)
  await expect(page.getByRole('status').first()).toHaveText('Live')
})
