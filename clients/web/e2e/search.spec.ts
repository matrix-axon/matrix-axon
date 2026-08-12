import {
  expect,
  request as apiRequest,
  test,
  type Page,
} from '@playwright/test'
import { openRoom, ROOM_URL, shortcutForProject } from './helpers'

/**
 * Message search (ADR 0066, M-W10). A real browser earns its keep here: the
 * `/` chord and its modifier twin ride real focus, the overlay's open state
 * is a history entry the Back button must pop, IntersectionObserver — absent
 * from jsdom by design — drives the auto-paging, and the hit link lands on
 * the room's jump-and-reveal path end to end.
 *
 * Bodies carry a per-run nonce: the mock backend's timeline is shared state
 * that outlives a local rerun (`reuseExistingServer`), and a repeated body
 * would double the hit counts the second time around.
 */
test.describe.configure({ mode: 'serial' })

/**
 * Start from the seeded fixture rather than from whatever ran before.
 *
 * The nonce above keeps *hit counts* honest across reruns, but the forward-
 * paging test asserts on how much history sits between its hit and the present,
 * and that is not nonce-scoped: one mock process serves every project, so each
 * engine in a cross-browser run inherits the previous engine's sends and has
 * further to page. That is why that test passes alone and fails as the second
 * project. Dropping test-sent traffic once, up front, makes the fixture the
 * same for whichever engine is running.
 */
test.beforeAll(async () => {
  const context = await apiRequest.newContext({
    baseURL: test.info().project.use.baseURL,
  })
  expect((await context.post('/__e2e/reset-timeline')).ok()).toBe(true)
  await context.dispose()
})

const NONCE = `n${Date.now()}`

const dialog = (page: Page) =>
  page.getByRole('dialog', { name: 'Search messages' })

test('open with /, search, jump to the hit, and Back reopens the search', async ({
  page,
}) => {
  await openRoom(page)
  // Something to find, put there the way anything gets there: a send.
  const composer = page.getByRole('textbox', { name: /^Message/ })
  await composer.fill(`the deploy failed on main ${NONCE}`)
  await composer.press('Enter')
  // Let the echo reconcile before the next send: a fill racing the
  // confirmation re-render can lose the keystrokes.
  await expect(page.locator('.event-row.pending')).toHaveCount(0)
  await composer.fill('unrelated chatter')
  await composer.press('Enter')
  await expect(page.locator('[data-event-id]').last()).toContainText(
    'unrelated chatter',
  )
  await expect(page.locator('.event-row.pending')).toHaveCount(0)

  // `/` never fires from the composer (it must type a slash); park focus on
  // the page first.
  await page.locator('.timeline').click({ position: { x: 20, y: 20 } })
  await page.keyboard.press('/')
  await expect(dialog(page)).toBeVisible()
  await expect(page).toHaveURL(/\?search=/)

  await page.getByLabel('Search query').fill(`deploy failed ${NONCE}`)
  await page.keyboard.press('Enter')

  const hit = page.locator('a.search-hit')
  await expect(hit).toHaveCount(1)
  await expect(hit.locator('mark').first()).toHaveText('deploy')
  await expect(dialog(page)).toContainText('1 result')

  // The URL carries the canonical token string: shareable, restorable.
  const url = decodeURIComponent(page.url()).replaceAll('+', ' ')
  expect(url).toContain('room:!room:hs')
  expect(url).toContain(`deploy failed ${NONCE}`)

  // Click the hit: overlay closes, the room reveals and highlights the event.
  await hit.click()
  await expect(dialog(page)).toHaveCount(0)
  await expect(page).toHaveURL(/\?event=/)
  await expect(page.locator('.event-row.highlighted')).toContainText(
    'the deploy failed on main',
  )

  // The topbar button restores the cached query and hits, rather than opening
  // an empty search overlay.
  await page.getByRole('button', { name: 'Search messages' }).click()
  await expect(dialog(page)).toBeVisible()
  await expect(page.locator('a.search-hit')).toHaveCount(1)

  // First Back closes the restored overlay; the next one pops to the original
  // search URL, which also restores its results.
  await page.goBack()
  await expect(dialog(page)).toHaveCount(0)
  await page.goBack()
  await expect(dialog(page)).toBeVisible()
  await expect(page.locator('a.search-hit')).toHaveCount(1)
})

test('the modifier search shortcut opens search from the composer; Escape closes it', async ({
  page,
}, testInfo) => {
  await openRoom(page)
  const composer = page.getByRole('textbox', { name: /^Message/ })
  await composer.focus()
  await page.keyboard.press(
    shortcutForProject(testInfo.project.name, 'Control+Shift+F', 'Meta+G'),
  )
  await expect(dialog(page)).toBeVisible()

  // Visible ≠ listening: Preact binds the overlay's Escape after paint, so
  // an immediate keypress can slip under it. Retrying the press is the same
  // second tap a human would give it.
  await expect(async () => {
    await page.keyboard.press('Escape')
    expect(await dialog(page).count()).toBe(0)
  }).toPass()
  expect(new URL(page.url()).search).toBe('')
})

test('/search composer command opens prefilled and scoped to the room', async ({
  page,
}) => {
  await openRoom(page)
  const composer = page.getByRole('textbox', { name: /^Message/ })
  await composer.fill(`/search deploy ${NONCE}`)
  await composer.press('Enter')

  await expect(dialog(page)).toBeVisible()
  // Scope defaulted to the invoking room: the scope select shows it.
  await expect(dialog(page).locator('.search-scope select')).toHaveValue(
    'current',
  )
  await expect(page.locator('a.search-hit')).toHaveCount(1)
})

test('scroll auto-pages via the IntersectionObserver sentinel', async ({
  page,
}) => {
  await openRoom(page)
  // 60 sends would be slow through the UI; hit the API like the app does.
  await page.evaluate(async (nonce) => {
    for (let i = 0; i < 60; i += 1) {
      await fetch(
        '/v1/accounts/11111111-1111-4111-8111-111111111111/rooms/%21room%3Ahs/send',
        {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ body: `pageme${nonce} number ${i}` }),
        },
      )
    }
  }, NONCE)

  await page.goto(`${ROOM_URL}?search=${encodeURIComponent(`pageme${NONCE}`)}`)
  await expect(dialog(page)).toContainText('60 results')
  // One page loaded (50), sentinel below; scrolling it into view fetches the
  // rest without a click.
  await expect(page.locator('a.search-hit')).toHaveCount(50)
  await page.locator('.search-load-more').scrollIntoViewIfNeeded()
  await expect(page.locator('a.search-hit')).toHaveCount(60)
  await expect(page.locator('.search-load-more')).toHaveCount(0)
})

test('a jumped timeline pages forward back to the present', async ({
  page,
}) => {
  // Depends on the fixtures the earlier tests planted: the nonce'd deploy
  // message with newer traffic (chatter, pageme sends) above it.
  await openRoom(page)
  await page.goto(
    `${ROOM_URL}?search=${encodeURIComponent(`deploy failed ${NONCE}`)}`,
  )
  // A *cold* deep link (the shared-URL case): the jump owns the initial
  // load, so the slice is parked at the hit instead of the live tail. An
  // in-room click with the event already loaded never needs to page.
  const href = await page.locator('a.search-hit').getAttribute('href')
  await page.goto(href!)
  await expect(page.locator('.event-row.highlighted')).toBeVisible()

  // The slice is parked at the hit and pages forward from there — via the
  // IntersectionObserver sentinel (which can fire the moment the reveal
  // scroll puts it in view) plus scrolling below. The "Load newer messages"
  // affordance is transient by design, so assert the end state rather than
  // racing the observer for a glimpse of the button.
  await expect(async () => {
    const button = page.getByRole('button', { name: 'Load newer messages' })
    if (await button.count()) {
      await button.scrollIntoViewIfNeeded()
    }
    expect(await button.count()).toBe(0)
  }).toPass()
  await expect(
    page.locator('[data-event-id]', { hasText: `pageme${NONCE} number 59` }),
  ).toBeAttached()
})

test('a search-disabled server renders the unavailable state', async ({
  page,
  request,
}) => {
  await request.post('/__e2e/search-503?disabled=true')
  try {
    await openRoom(page)
    await page.goto(`${ROOM_URL}?search=${encodeURIComponent('anything')}`)
    await expect(dialog(page)).toContainText(
      'Search is not enabled on this server.',
    )
  } finally {
    await request.post('/__e2e/search-503?disabled=false')
  }
})
