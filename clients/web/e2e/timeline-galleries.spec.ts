import { expect, test } from '@playwright/test'
import { LAST_ROOM_URL, ROOM_URL, openRoom, signIn } from './helpers'

/**
 * Timeline galleries (ADR 0081). The seed room carries a run of four images
 * from one sender, seconds apart — the third deliberately encrypted with no
 * sender thumbnail and a size that exhausts the run's byte budget on its own,
 * which is the case that must defer rather than pull a full-size object to
 * draw a 320px square.
 *
 * What jsdom cannot prove and this can: that the grid actually lays out as a
 * fixed three-column square grid, that a deferred cell issues no request until
 * asked, and that a gallery row is a real `li.event-row` in the same `<ol>` as
 * every other row — which is what scroll anchoring depends on.
 *
 * Written order-agnostically. `e2e/mock-server.mjs` serves its two GET
 * branches in opposite orders (the `at_ts` path newest-first, the plain path
 * oldest-first), so the seeded run renders reversed; asserting "the third
 * image defers" would pin that quirk rather than this feature.
 */

/** Cells only exist once their bytes load, which needs them on screen. */
async function showGallery(page: import('@playwright/test').Page) {
  const gallery = page.locator('.gallery-row')
  await expect(gallery).toHaveCount(1)
  await gallery.scrollIntoViewIfNeeded()
  return gallery
}

test('a run of images renders as one gallery row', async ({ page }) => {
  await openRoom(page)

  const gallery = await showGallery(page)
  await expect(gallery.locator('.gallery-cell')).toHaveCount(4)

  // One `li.event-row` in the timeline's own list, not a nested structure:
  // `captureAnchor` binary-searches those assuming document order is
  // monotonic in vertical position.
  const shape = await gallery.evaluate((row) => ({
    tag: row.tagName,
    parent: row.parentElement?.tagName ?? '',
    parentClass: row.parentElement?.className ?? '',
    eventId: row.getAttribute('data-event-id'),
  }))
  expect(shape.tag).toBe('LI')
  expect(shape.parent).toBe('OL')
  expect(shape.parentClass).toBe('event-list')
  // The run's first *rendered* event, whichever end the mock serves from.
  expect(shape.eventId).toMatch(/^\$gallery-\d:hs$/)

  // Standalone images keep their own full-width rows.
  await expect(page.locator('.media-figure')).toHaveCount(3)
})

test('cells are square and laid out in three fixed columns', async ({
  page,
}) => {
  await openRoom(page)
  await showGallery(page)
  const grid = page.locator('.gallery-grid')
  await expect(grid).toBeVisible()

  const layout = await grid.evaluate((el) => {
    const columns = getComputedStyle(el).gridTemplateColumns.split(/\s+/)
    const cells = [...el.querySelectorAll('.gallery-cell')].map((cell) => {
      const rect = cell.getBoundingClientRect()
      return Math.abs(rect.width - rect.height) < 1.5
    })
    return { columnCount: columns.length, allSquare: cells.every(Boolean) }
  })

  // Fixed three columns, so a run's height is `ceil(n / 3)` cells regardless
  // of container width — the space it reserves cannot depend on layout.
  expect(layout.columnCount).toBe(3)
  expect(layout.allSquare).toBe(true)
})

test('a large encrypted image waits for a tap instead of loading itself', async ({
  page,
}) => {
  const mediaRequests: string[] = []
  page.on('request', (request) => {
    const url = request.url()
    if (url.includes('/v1/media/')) {
      mediaRequests.push(url)
    }
  })

  await openRoom(page)
  await showGallery(page)
  const deferred = page.locator('.gallery-cell-deferred')
  await expect(deferred).toHaveCount(1)

  // Named for what it does, never impersonating a loaded image. Its position
  // depends on which end the mock serves from, so only the shape is pinned.
  await expect(deferred).toHaveAttribute(
    'aria-label',
    // Whichever of the two encrypted images the run reaches second; the
    // mock does not order its GET branches consistently.
    /^Load image \d of 4, gallery-[12]\.png, [\d.]+ MB$/,
  )
  const deferredId = (await deferred.getAttribute('aria-label'))!.match(
    /gallery-(\d)/,
  )![1]
  expect(
    mediaRequests.some((url) => url.includes(`gallery${deferredId}`)),
  ).toBe(false)

  await deferred.click()
  await expect
    .poll(() =>
      mediaRequests.some((url) => url.includes(`gallery${deferredId}`)),
    )
    .toBe(true)
})

test('ungrouping shows the images as ordinary rows, and it sticks', async ({
  page,
}) => {
  await openRoom(page)
  await showGallery(page)
  await page.getByRole('button', { name: 'Ungroup images' }).click()

  await expect(page.locator('.gallery-row')).toHaveCount(0)
  // The four images now render as ordinary rows alongside the three
  // standalone ones.
  await expect(page.locator('.media-figure')).toHaveCount(7)
  await expect(
    page.getByRole('button', { name: 'Image gallery' }),
  ).toBeVisible()

  // Leaving the room and returning keeps the choice for this session.
  // Deliberately in-app navigation: `page.goto` is a full load, which resets
  // the memory by design — it is scoped to the page, not to storage.
  const otherRoom = page.locator(`a[href="${LAST_ROOM_URL}"]`)
  await otherRoom.scrollIntoViewIfNeeded()
  await otherRoom.click()
  await expect(page.locator('.gallery-row')).toHaveCount(0)

  await page.goBack()
  await expect(page.locator('.gallery-row')).toHaveCount(0)
  await expect(
    page.getByRole('button', { name: 'Image gallery' }),
  ).toBeVisible()
})

test('a deep link into a gallery keeps it grouped and centres the cell', async ({
  page,
}) => {
  // Forcing the jump target back to an ordinary row was the first approach,
  // and it tore the gallery apart for a link that pointed *into* it.
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto(`${ROOM_URL}?event=%24gallery-2%3Ahs`)
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )

  // Still one gallery, not split around the target.
  await expect(page.locator('.gallery-row')).toHaveCount(1)
  await expect(page.locator('.gallery-cell')).toHaveCount(4)

  const highlighted = page.locator('.gallery-cell.highlighted')
  await expect(highlighted).toHaveCount(1)
  await expect(highlighted).toHaveAttribute('data-event-id', '$gallery-2:hs')

  // And the target is actually on screen, which is the point of the link.
  const onScreen = await highlighted.evaluate((cell) => {
    const rect = cell.getBoundingClientRect()
    return rect.top >= 0 && rect.bottom <= window.innerHeight
  })
  expect(onScreen).toBe(true)
})

test('a gallery cell opens the viewer at that image', async ({ page }) => {
  await openRoom(page)
  await showGallery(page)

  const cells = page.locator('.gallery-cell-open')
  await expect(cells.first()).toBeVisible()

  // Which run position this cell holds, taken from the cell itself: one cell
  // in the fixture is deferred, so the nth *open* cell is not the nth image.
  const cell = cells.nth(1)
  const label = (await cell.getAttribute('aria-label')) ?? ''
  const position = /Image (\d) of 4/.exec(label)?.[1]
  expect(position).toBeDefined()

  await cell.click()
  const dialog = page.getByRole('dialog')
  await expect(dialog).toBeVisible()
  // Position within the post, not among every loaded image.
  await expect(dialog.getByRole('status')).toContainText(`${position}/4`)
})
