import { expect, test } from '@playwright/test'
import { ACCOUNT_ID, ROOM_ID, signIn } from './helpers'

/**
 * The room list is windowed: a power user in thousands of rooms should have
 * only the visible slice in the DOM. jsdom has no layout, so the unit tests
 * exercise the render-everything fallback and this lane is the only place the
 * windowing itself runs.
 */

const BULK = 1500

test.beforeEach(async ({ page, request }) => {
  await request.post(`/__e2e/bulk-rooms?count=${BULK}`)
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
})

test.afterEach(async ({ request }) => {
  await request.post('/__e2e/bulk-rooms?count=0')
})

const rows = (page: import('@playwright/test').Page) =>
  page.locator('.room-list a.room-link')

test('renders a slice of the list, not all of it', async ({ page }) => {
  await page.goto('/')
  await expect(rows(page).first()).toBeVisible()

  const rendered = await rows(page).count()
  // A 900px-tall viewport fits well under 30 rows even with overscan.
  expect(rendered).toBeGreaterThan(0)
  expect(rendered).toBeLessThan(40)

  // The scrollbar must still reflect all 1503 rooms, or scrolling is a lie.
  const { listHeight, rowHeight } = await page.evaluate(() => {
    const list = document.querySelector('.room-list') as HTMLElement
    const row = document.querySelector('li.room-row') as HTMLElement
    return {
      listHeight: list.getBoundingClientRect().height,
      rowHeight: row.getBoundingClientRect().height,
    }
  })
  expect(listHeight).toBeGreaterThan(rowHeight * 1500)
})

test('scrolling reveals later rooms and retires earlier ones', async ({
  page,
}) => {
  await page.goto('/')
  await expect(rows(page).first()).toBeVisible()
  await expect(page.getByText('E2E Room')).toBeVisible()

  const roomListPane = page.locator('.room-list-pane')
  await roomListPane.evaluate((el) => {
    el.scrollTop = el.scrollHeight
  })

  // The last room by recent-activity sort is the oldest bulk room.
  await expect(
    page.getByText(`Bulk Room ${String(BULK - 1).padStart(4, '0')}`),
  ).toBeVisible()
  // …and the first row is gone from the DOM entirely, not merely offscreen.
  await expect(page.getByText('E2E Room')).toHaveCount(0)
  expect(await rows(page).count()).toBeLessThan(40)
})

test('keyboard roving walks past the window edge', async ({ page }) => {
  await page.goto('/')
  await expect(rows(page).first()).toBeVisible()

  // Ctrl-K focuses the name filter — deferred to a macrotask, so wait for it
  // before the ArrowDown that enters the list at row 0.
  await page.keyboard.press('Control+k')
  await expect(page.locator('input.name-filter')).toBeFocused()
  await page.keyboard.press('ArrowDown')
  await expect(page.locator('a.room-link[data-index="0"]')).toBeFocused()

  // Walk far enough to leave the initially-rendered window behind.
  for (let i = 0; i < 30; i++) {
    await page.keyboard.press('ArrowDown')
  }
  await expect(page.locator('a.room-link[data-index="30"]')).toBeFocused()
  // The row it scrolled to must actually be on screen.
  await expect(page.locator('a.room-link[data-index="30"]')).toBeInViewport()
})

/**
 * At single-pane widths a room hides the sidebar with `display:none`. Its
 * height is then 0, which is a *hidden* scroll container, not an immeasurable
 * one — the list used to read that as "cannot window" and mount all 1503 rows
 * into a pane nobody could see, on exactly the devices least able to afford it.
 */
test('a phone-width room page mounts no room rows at all', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/')
  await expect(rows(page).first()).toBeVisible()
  expect(await rows(page).count()).toBeLessThan(40)

  await page.goto(`/${ACCOUNT_ID}/rooms/${encodeURIComponent(ROOM_ID)}`)
  // At phone width the room title lives in the topbar chrome; the in-page
  // `.room-head` heading is display:none below the single-pane breakpoint.
  await expect(page.locator('.topbar-room-title')).toHaveText('E2E Room')
  await expect(page.locator('.sidebar')).toBeHidden()
  await expect(rows(page)).toHaveCount(0)

  // Showing it again must restore a live, scrollable window.
  await page.goto('/')
  await expect(rows(page).first()).toBeVisible()
  expect(await rows(page).count()).toBeLessThan(40)
  await page.locator('.sidebar').evaluate((el) => {
    el.scrollTop = el.scrollHeight
  })
  await expect(
    page.getByText(`Bulk Room ${String(BULK - 1).padStart(4, '0')}`),
  ).toBeVisible()
})

test('the pinned separator lands on the pinned/unpinned boundary', async ({
  page,
}) => {
  await page.goto('/')
  await expect(rows(page).first()).toBeVisible()

  // Pin the second row; it floats to the top and the separator appears below it.
  await page.locator('li.room-row').nth(1).getByRole('button').click()

  const separator = page.locator('.room-separator')
  await expect(separator).toHaveCount(1)

  const { sepTop, secondRowTop } = await page.evaluate(() => {
    const sep = document.querySelector('.room-separator')!
    const second = document.querySelectorAll('li.room-row')[1]
    return {
      sepTop: sep.getBoundingClientRect().top,
      secondRowTop: second.getBoundingClientRect().top,
    }
  })
  // One room is pinned, so the separator sits exactly on top of row index 1.
  expect(Math.abs(sepTop - secondRowTop)).toBeLessThan(1.5)
})
