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
 * Hide the tab, let `awayMs` appear to pass, and return to it.
 *
 * The policy measures absence with `Date.now()`, so faking the clock is what
 * lets the test cross the one-minute "the user was away" threshold without
 * waiting a minute.
 *
 * The two dispatches are deliberately in separate `evaluate` calls, and the
 * return is fired from a timer: the reload this provokes can now begin
 * *synchronously* inside the handler, which destroys the execution context of
 * the `evaluate` that dispatched it ("Execution context was destroyed, most
 * likely because of a navigation"). It could not before ADR 0096, because the
 * pre-reload draft flush always awaited a network round trip; the same flush now
 * usually finds nothing pending, since `connectDeviceStateFlush` already sent it
 * on the way out.
 *
 * State lives on `window` because it has to survive between the two calls.
 *
 * **The awaited call resolves before the dispatch runs.** That is inherent: the
 * dispatch may destroy the context it would have to resolve in, which is the
 * whole reason it is deferred. Callers that expect a reload must use
 * `returnAfterAwayExpectingReload`, which arms `load` first — the same pattern
 * as `reloadFromInsidePage`. `expect.poll(() => page.evaluate(...))` does not
 * retry a throw from the callback (Playwright only retries matcher failures),
 * so a navigation mid-evaluate fails the poll immediately. Locator assertions
 * retry across a navigation, but they can still sample the outgoing document;
 * waiting for `load` puts them on the new one.
 */
async function returnAfterAway(page: Page, awayMs: number): Promise<void> {
  await page.evaluate(() => {
    const realNow = Date.now.bind(Date)
    const state = { skew: 0, hidden: true }
    ;(window as unknown as { __away: typeof state }).__away = state
    Date.now = () => realNow() + state.skew

    Object.defineProperty(document, 'hidden', {
      configurable: true,
      get: () => state.hidden,
    })
    document.dispatchEvent(new Event('visibilitychange'))
  })

  await page.evaluate((ms) => {
    const state = (
      window as unknown as { __away: { skew: number; hidden: boolean } }
    ).__away
    state.skew = ms
    state.hidden = false
    setTimeout(() => document.dispatchEvent(new Event('visibilitychange')), 0)
  }, awayMs)
}

/**
 * `returnAfterAway` plus a wait for the in-page `location.reload()` it is
 * expected to provoke. The `load` listener is armed in the same `Promise.all`
 * as the away/return sequence, matching `reloadFromInsidePage`: the reload is
 * async (update check, then a draft flush of up to `FLUSH_TIMEOUT_MS`), so
 * attaching after `returnAfterAway` resolves can miss it, and a thrown helper
 * must not leave the `load` waiter as an unhandled rejection.
 *
 * The away/return evaluates can themselves be killed by that reload — hiding
 * the tab is enough when an update is already known — so a destroyed context
 * there is success, not a failed helper.
 */
async function returnAfterAwayExpectingReload(
  page: Page,
  awayMs: number,
): Promise<void> {
  await Promise.all([
    page.waitForEvent('load', { timeout: 15_000 }),
    returnAfterAway(page, awayMs).catch((error: unknown) => {
      if (
        !(error instanceof Error) ||
        !error.message.includes('Execution context was destroyed')
      ) {
        throw error
      }
    }),
  ])
}

test.afterEach(async ({ page }) => {
  await clearDeploy(page)
})

test('a tab on the current build neither warns nor reloads', async ({
  page,
}) => {
  await signIn(page)
  await page.goto(ROOM_URL)
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )

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
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )

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
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )

  await stageDeploy(page, 'build-from-e2e')
  await page.request.post('/__e2e/drop-sockets')
  await expect(
    page.getByText('A new version of Axon is available'),
  ).toBeVisible({ timeout: 15_000 })

  // Clear the override first: the reload should land on the real build and the
  // banner should be gone, which is what "the update applied" looks like.
  await clearDeploy(page)
  await page.getByRole('button', { name: 'Reload' }).click()

  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )
  await expect(
    page.getByText('A new version of Axon is available'),
  ).toHaveCount(0)
})

test('a backgrounded tab reloads itself on return', async ({ page }) => {
  await signIn(page)
  await page.goto(ROOM_URL)
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )

  await page.evaluate(() => {
    window.__reloaded = true
  })

  await stageDeploy(page, 'build-from-e2e')
  await returnAfterAwayExpectingReload(page, 90_000)

  // A navigation clears the page context, so the marker is gone.
  expect(await page.evaluate(() => window.__reloaded === true)).toBe(false)
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
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )
  await returnAfterAwayExpectingReload(page, 90_000)

  // Give a loop time to show itself: a settling window *after* the permitted
  // reload, in which a broken guard would fire again on every check.
  await page.waitForTimeout(5000)

  // The initial goto plus at most the one permitted reload.
  expect(navigations).toBeLessThanOrEqual(2)
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )
})

/**
 * The reload this module performs is an in-page `location.reload()`, so every
 * engine classifies it as a reload and the ordinary startup thread scrub applies
 * — no special case anywhere in the app. This pins that, because an automatic
 * reload restoring a thread panel the user did not ask for is the one way this
 * feature could quietly change what the app looks like after an update.
 *
 * Declared last on purpose. Under `mode: 'serial'` a failure skips every test
 * declared after it, so a slow reload here would have silently dropped the
 * loop-guard regression above from the run — the one test in this file whose
 * absence nobody would notice, because a reload loop is what it exists to catch.
 *
 * `returnAfterAwayExpectingReload` waits for the document `load` (the update
 * check is async and the draft flush can take up to `FLUSH_TIMEOUT_MS`). The
 * assertions after that still get a 15s budget: the timeline has to mount and
 * paint again, and the 5s default is not enough headroom on a loaded runner.
 */
test('an automatic reload closes the restored thread view', async ({
  page,
}) => {
  // The 15s `load` wait plus three 15s assertion budgets stack sequentially,
  // and the per-test default is 30s (`playwright.config.ts` sets no top-level
  // `timeout`), so the test itself would expire first and report a generic
  // "Test timeout of 30000ms exceeded" instead of the assertion the budget
  // exists to name. Raised past those waits plus the setup ahead of them.
  test.setTimeout(90_000)

  await signIn(page)
  await page.goto(`${ROOM_URL}?thread=%24root`)
  await expect(page.locator('.thread-panel')).toBeVisible()

  await stageDeploy(page, 'build-from-e2e')
  await returnAfterAwayExpectingReload(page, 90_000)

  await expect(page).not.toHaveURL(/thread=/, { timeout: 15_000 })
  await expect(page.locator('.thread-panel')).toHaveCount(0, {
    timeout: 15_000,
  })
  await expect(page.locator('.timeline')).toBeVisible({ timeout: 15_000 })
})
