import { expect, test } from '@playwright/test'
import { reloadFromInsidePage, signIn, waitForRoomListCache } from './helpers'

/**
 * The ADR 0085 phase 2 boot summary, proven to exist and to carry real numbers
 * in a real browser — because its whole purpose is to be read off a screen
 * recording from a phone with no console, and a summary that quietly reports
 * `null` there would be indistinguishable from one that reports the truth.
 */
// The mock is one process shared by every spec, so the hold must not outlive
// this file — and it must be reset even when the test *fails*, or the delay
// leaks into every spec that follows. A first version reset it on the happy
// path only, and one failure here turned into failures three specs away.
test.afterEach(async ({ page }) => {
  await page.request.post('/__e2e/rooms-delay?hold=none')
})

test('the boot summary reports the cache winning the race', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })

  // Instrumentation is enabled through **Settings**, deliberately, not
  // `?perf=1`. The URL flag latches inside `perfEnabled()` before any store
  // exists, so it masks the ordering this test exists to guard: the stored
  // preference has to be applied while the service graph is built, or the
  // cache-read marks fire into a disabled recorder and the summary reports
  // `read=null` — which is precisely what a device reported while this spec
  // was passing.
  await page.goto('/settings')
  await page.getByRole('button', { name: 'Debug' }).click()
  await page.getByLabel('Performance instrumentation').check()
  await page.goto('/')
  await expect(page.getByText('E2E Room')).toBeVisible()
  // A painted row means the refresh settled, not that the write-back landed —
  // that is one more async hop. Reloading before it lands gives a cold cache,
  // and then every assertion below reads a summary that was never emitted.
  await waitForRoomListCache(page)

  // Model an app *relaunch* for the measured load, not a navigation.
  // `setPerfEnabled` mirrors the flag into `sessionStorage`, which survives
  // within a tab and re-latches instrumentation before any store is built —
  // masking the ordering this test guards. Closing the app clears it, leaving
  // only the localStorage preference, which is the path a device takes.
  //
  // It has to be cleared immediately before the load being measured: any
  // intervening load re-writes the flag from `App`'s effect, and the next one
  // is instrumented from the start again.
  await page.evaluate(() => sessionStorage.clear())

  // Hold the room list open so the cached arm has something to beat.
  expect(
    (await page.request.post('/__e2e/rooms-delay?hold=typical')).ok(),
  ).toBe(true)
  // In-page rather than `page.reload()`: the summary asserted below carries
  // `nav`, which is the Navigation Timing type, and the driver's reload is
  // reported as `navigate` under Firefox. See `reloadFromInsidePage`.
  await reloadFromInsidePage(page)
  await expect(page.getByText('E2E Room')).toBeVisible()
  // Visible *then* hidden. Waiting only for hidden passes instantly when the
  // affordance never appeared at all — a cold cache would sail through it, and
  // the summary this test exists to read would not exist yet.
  await expect(page.getByText('Updating…')).toBeVisible()
  // The summary is emitted when the *refresh* settles, which the hold defers.
  await expect(page.getByText('Updating…')).toBeHidden({ timeout: 10_000 })

  const summary = () =>
    page.evaluate(() =>
      performance
        .getEntriesByType('mark')
        .filter((mark) => mark.name === 'axon:boot:room-list')
        .map((mark) => (mark as PerformanceMark).detail)
        .at(-1),
    )
  await expect.poll(summary, { timeout: 10_000 }).toBeTruthy()
  const detail = (await summary()) as {
    nav: string
    hydrate: number | null
    read: number | null
    boot: number | null
    rows: number | null
    net: number | null
    saved: number | null
    rooms: number | null
  }
  // Tightened back from `['reload', 'navigate']`. That widening was added here
  // for a Firefox quirk that turned out not to exist: what reports `navigate` is
  // Playwright's *driver* reload, not the engine, and this test now reloads from
  // inside the page (#131), which all three engines classify as `reload`.
  // Accepting `navigate` would silently pass a misclassified reload again.
  expect(detail.nav).toBe('reload')
  // Every field the readout shows has to be a number, not a null the phone
  // would render as a dash.
  expect(detail.hydrate).not.toBeNull()
  expect(detail.rows).not.toBeNull()
  expect(detail.net).not.toBeNull()
  expect(detail.rooms).toBeGreaterThan(0)
  // The decomposition of `hydrate` into startup and storage. Both must be
  // real numbers via the Settings path, not just under `?perf=1`.
  expect(detail.read).not.toBeNull()
  expect(detail.boot).not.toBeNull()
  // Rows painted before the held response settled — the claim of the phase.
  expect(detail.saved).toBeGreaterThan(0)
  expect(detail.rows!).toBeLessThan(detail.net!)
})
