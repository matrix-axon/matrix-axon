import { expect, test, type Page } from '@playwright/test'
import { openRoom, ROOM_URL, signIn } from './helpers'

/**
 * Lightbox paging across the loaded timeline (ADR 0081). The seed room carries
 * three `m.image` events, deliberately spread across senders and hours so they
 * stay three separate rows.
 *
 * What jsdom cannot prove and this can: that paging swaps the decoded bytes in
 * a real browser, and that focus lands on the right row after a real Escape.
 *
 * Deliberately written against *rendered* order rather than timestamps. The
 * viewer's contract is that its sequence matches the order the timeline draws
 * (see `imageSequence`), and `e2e/mock-server.mjs` does not order its two GET
 * branches consistently — the `at_ts` path sorts newest-first while the plain
 * path returns the seeded array oldest-first. Asserting "the third image is
 * 3 of 3" would be pinning that inconsistency rather than this feature.
 */

/** The `data-event-id`s of the image rows, in the order the timeline draws. */
function imageRowIds(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    [...document.querySelectorAll('[data-event-id]')]
      .filter((row) => row.querySelector('.media-figure') !== null)
      .map((row) => row.getAttribute('data-event-id') ?? ''),
  )
}

/**
 * Open the viewer at a specific event rather than at "the last image on the
 * page". Media loads lazily, so `.media-image img` only matches images whose
 * bytes have arrived — an index into it silently depends on scroll position.
 */
async function openImage(page: Page, eventId: string): Promise<void> {
  const row = page.locator(`[data-event-id="${eventId}"]`)
  await row.scrollIntoViewIfNeeded()
  const opener = row.locator('.media-open')
  await expect(opener).toBeEnabled()
  await opener.click()
}

test('pages across the timeline images and announces position', async ({
  page,
}) => {
  await openRoom(page)
  const ids = await imageRowIds(page)
  expect(ids).toHaveLength(3)

  await openImage(page, ids[0])
  const dialog = page.getByRole('dialog')
  await expect(dialog).toBeVisible()

  // The three seeded images are deliberately *not* one gallery, so there is
  // no counter — only a position within a run is shown, and an index among
  // all loaded images was dropped as useless to the reader.
  await expect(dialog.getByRole('status')).not.toContainText('of 3')

  // The oldest end is genuinely reached (the mock sends no cursor), so prev
  // is inert and says so.
  await expect(page.locator('.lightbox-prev')).toBeDisabled()
  await expect(dialog.getByRole('status')).toContainText('oldest image')

  const shown = () =>
    dialog
      .locator('img')
      .getAttribute('src')
      .then((src) => src ?? '')
  const first = await shown()

  await page.locator('.lightbox-next').click()
  // A different blob, not the same element mutated in place.
  await expect.poll(shown).not.toBe(first)

  // Stepping off the oldest end re-enables prev; deliberately not asserted
  // against a total, because the seeded gallery adds to the sequence and a
  // fixed step count would pin the fixture rather than the behaviour.
  await expect(page.locator('.lightbox-prev')).toBeEnabled()

  await page.keyboard.press('ArrowLeft')
  await expect(page.locator('.lightbox-prev')).toBeDisabled()
  await expect.poll(shown).toBe(first)
})

test('renders contextual action icons on desktop', async ({ page }) => {
  await openRoom(page)
  const ids = await imageRowIds(page)
  await openImage(page, ids[0])

  const icons = page.locator('.lightbox-action .event-action-icon')
  await expect(icons).toHaveCount(3)
  expect(
    await icons.evaluateAll((elements) =>
      elements.map((element) => getComputedStyle(element).display),
    ),
  ).toEqual(['block', 'block', 'block'])
})

test('returns focus to the image it was left on, not the one it opened from', async ({
  page,
}) => {
  await openRoom(page)
  const ids = await imageRowIds(page)

  await openImage(page, ids[0])
  const dialog = page.getByRole('dialog')
  await expect(dialog).toBeVisible()

  // Stepped with the button rather than the keyboard. `useShortcuts`
  // subscribes in an effect, and Preact flushes effects *after* paint, so a
  // key pressed the instant the dialog appears can miss the listener — a race
  // no human can hit, but one Playwright hits a few percent of the time. Arrow
  // paging has its own test; this one is about where focus lands.
  await page.locator('.lightbox-next').click()
  await expect(page.locator('.lightbox-prev')).toBeEnabled()
  await page.keyboard.press('Escape')
  await expect(dialog).toBeHidden()

  // Polled, not read once: focus is restored in the modal's unmount cleanup,
  // which can land a tick after the dialog stops being visible.
  const focusedRow = () =>
    page.evaluate(
      () =>
        document.activeElement
          ?.closest('[data-event-id]')
          ?.getAttribute('data-event-id') ?? 'none',
    )
  await expect.poll(focusedRow).toBe(ids[1])
  expect(await focusedRow()).not.toBe(ids[0])
})

test('a tap on the image hides the chrome, and another restores it', async ({
  page,
}) => {
  await openRoom(page)
  const ids = await imageRowIds(page)
  await openImage(page, ids[0])

  const overlay = page.locator('.lightbox')
  await expect(overlay).not.toHaveClass(/lightbox-immersive/)

  await page.locator('.lightbox-image').click()
  await expect(overlay).toHaveClass(/lightbox-immersive/)
  // Hidden, not unmounted — the focus trap must keep something to trap.
  await expect(page.locator('.lightbox-close')).toHaveCount(1)
  await expect(page.getByRole('dialog')).toBeVisible()

  // Past the double-tap grace, so this counts as a fresh gesture.
  await page.waitForTimeout(400)
  await page.locator('.lightbox-image').click()
  await expect(overlay).not.toHaveClass(/lightbox-immersive/)
})

test('a double-click toggles the chrome once, not twice', async ({ page }) => {
  // Reported from real use: the chrome flashed back and vanished again. Two
  // clicks milliseconds apart toggled twice, so the buttons appeared
  // impossible to bring back. Only a real browser produces the pair.
  await openRoom(page)
  const ids = await imageRowIds(page)
  await openImage(page, ids[0])

  const overlay = page.locator('.lightbox')
  await page.locator('.lightbox-image').dblclick()
  await expect(overlay).toHaveClass(/lightbox-immersive/)
  // Settled, not flickering: still hidden well past the grace.
  await page.waitForTimeout(400)
  await expect(overlay).toHaveClass(/lightbox-immersive/)

  // A deliberate tap still brings them back.
  await page.locator('.lightbox-image').click()
  await expect(overlay).not.toHaveClass(/lightbox-immersive/)
})

test('tabbing reveals hidden chrome instead of stranding focus', async ({
  page,
}) => {
  await openRoom(page)
  const ids = await imageRowIds(page)
  await openImage(page, ids[0])

  const overlay = page.locator('.lightbox')
  await page.locator('.lightbox-image').click()
  await expect(overlay).toHaveClass(/lightbox-immersive/)

  await page.keyboard.press('Tab')
  await expect(overlay).not.toHaveClass(/lightbox-immersive/)
  // Focus stayed inside the dialog: the trap held even while chrome was hidden.
  const inside = await page.evaluate(
    () =>
      document.querySelector('.lightbox')?.contains(document.activeElement) ??
      false,
  )
  expect(inside).toBe(true)
})

test.describe('message actions on a phone', () => {
  test.use({ viewport: { width: 320, height: 640 }, hasTouch: true })

  test('keeps the contextual action toolbar within the viewport', async ({
    page,
  }) => {
    // `openRoom` intentionally sets a desktop viewport for its callers; this
    // phone-layout test must keep the viewport declared above.
    await signIn(page)
    await page.goto(ROOM_URL)
    await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
      'Live',
    )
    const ids = await imageRowIds(page)
    await openImage(page, ids[0])

    const toolbar = page.locator('.lightbox-toolbar')
    await expect(toolbar.getByRole('button', { name: 'Reply' })).toBeVisible()
    await expect(toolbar.getByRole('button', { name: 'Thread' })).toBeVisible()
    await expect(toolbar.getByRole('button', { name: 'React' })).toBeVisible()
    await expect(
      toolbar.getByRole('button', { name: 'Save image to device' }),
    ).toBeVisible()
    const visualLabels = await toolbar
      .locator('button')
      .evaluateAll((buttons) =>
        buttons
          .map((button) => ({
            label: button.getAttribute('aria-label'),
            left: button.getBoundingClientRect().left,
          }))
          .sort((a, b) => a.left - b.left)
          .map(({ label }) => label),
      )
    expect(visualLabels).toEqual([
      'Reply',
      'Thread',
      'React',
      'Save image to device',
      'Close',
    ])

    const layout = await toolbar.evaluate((element) => {
      const toolbarBox = element.getBoundingClientRect()
      const buttons = [...element.querySelectorAll('button')].map((button) =>
        button.getBoundingClientRect(),
      )
      return {
        toolbarBox,
        viewportWidth: window.innerWidth,
        buttons,
      }
    })
    expect(layout.toolbarBox.left).toBeGreaterThanOrEqual(0)
    expect(layout.toolbarBox.right).toBeLessThanOrEqual(layout.viewportWidth)
    for (const button of layout.buttons) {
      expect(button.width).toBeGreaterThanOrEqual(44)
      expect(button.height).toBeGreaterThanOrEqual(44)
    }

    // The real browser hit-test closes the viewer and leaves the target row's
    // established reaction picker usable, rather than leaving a fixed overlay
    // above it.
    await toolbar.getByRole('button', { name: 'React' }).click()
    await expect(page.getByRole('dialog')).toBeHidden()
    await expect(page.locator('.reaction-picker-shell')).toBeVisible()
  })

  test('keeps the six-control own-image toolbar within the viewport', async ({
    page,
  }) => {
    await page.request.post('/__e2e/lightbox-own-image?enabled=true')
    try {
      await signIn(page)
      await page.goto(ROOM_URL)
      await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
        'Live',
      )
      await openImage(page, '$seed-image-own:hs')

      const toolbar = page.locator('.lightbox-toolbar')
      await expect(
        toolbar.getByRole('button', { name: 'Delete' }),
      ).toBeVisible()
      const visualLabels = await toolbar
        .locator('button')
        .evaluateAll((buttons) =>
          buttons
            .map((button) => ({
              label: button.getAttribute('aria-label'),
              left: button.getBoundingClientRect().left,
            }))
            .sort((a, b) => a.left - b.left)
            .map(({ label }) => label),
        )
      expect(visualLabels).toEqual([
        'Reply',
        'Thread',
        'React',
        'Delete',
        'Save image to device',
        'Close',
      ])

      const layout = await toolbar.evaluate((element) => {
        const toolbarBox = element.getBoundingClientRect()
        const buttons = [...element.querySelectorAll('button')].map((button) =>
          button.getBoundingClientRect(),
        )
        return { toolbarBox, viewportWidth: window.innerWidth, buttons }
      })
      expect(layout.toolbarBox.left).toBeGreaterThanOrEqual(0)
      expect(layout.toolbarBox.right).toBeLessThanOrEqual(layout.viewportWidth)
      for (const button of layout.buttons) {
        expect(button.width).toBeGreaterThanOrEqual(44)
        expect(button.height).toBeGreaterThanOrEqual(44)
      }
    } finally {
      await page.request.post('/__e2e/lightbox-own-image?enabled=false')
    }
  })
})

/*
 * Still owed: the WebKit/iPhone 13 proof that a swipe inside the dialog pages
 * without the room's swipe-back stealing it. It belongs on WebKit
 * specifically — the collision it rules out is a UIKit recognizer — which
 * means extending the webkit project's `testMatch` in `playwright.config.ts`.
 * The non-collision is covered structurally by unit tests in
 * `media-viewer.test.tsx`, which assert swipe paging and the declined edge
 * band.
 */
