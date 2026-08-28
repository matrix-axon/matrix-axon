import { expect, test } from '@playwright/test'
import { openRoom, ROOM_URL, signIn } from './helpers'

/**
 * The state-events control moved from the room header into Settings, which
 * makes it a *persisted preference* rather than per-room view state, and became
 * a three-way tier (ADR 0083). Persistence through real `localStorage` and a
 * reload is the part unit tests (which inject an in-memory store) cannot prove.
 */
test.describe.configure({ mode: 'serial' })

test('the room header no longer carries a state-events checkbox', async ({
  page,
}) => {
  await openRoom(page)

  await expect(page.getByLabel('State events')).toHaveCount(0)
  await expect(page.getByRole('button', { name: 'Jump' })).toBeVisible()
})

test('the three state-event tiers filter the timeline, and survive a reload', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1400, height: 900 })
  await page.goto(ROOM_URL)
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )

  // The default tier renders the seeded join as a notice and hides the seeded
  // topic change (ADR 0083).
  await expect(page.locator('.event-row.state-event')).toHaveCount(1)
  await expect(page.getByText('@me joined the room')).toBeVisible()

  await page.goto('/settings')
  await expect(page.getByLabel('Membership and profile changes')).toBeChecked()
  await page.getByLabel('All state events').check()

  await page.goto(ROOM_URL)
  await expect(page.locator('.event-row.state-event')).toHaveCount(2)

  // The preference outlives the tab, unlike the checkbox it replaced.
  await page.reload()
  await expect(page.getByRole('status', { name: /WebSocket:/ })).toHaveText(
    'Live',
  )
  await expect(page.locator('.event-row.state-event')).toHaveCount(2)

  await page.goto('/settings')
  await expect(page.getByLabel('All state events')).toBeChecked()
  await page.getByLabel('Hidden').check()

  await page.goto(ROOM_URL)
  await expect(page.locator('.event-row.state-event')).toHaveCount(0)
})

test('the timeline checkboxes render as checkboxes, not full-width text fields', async ({
  page,
}) => {
  await signIn(page)
  await page.setViewportSize({ width: 1100, height: 700 })
  await page.goto('/settings')

  // The global `input` rule sizes text fields (width: 100%, padded, bordered)
  // and only radios and checkboxes are exempt.
  const box = await page.getByLabel('Hide deleted messages').evaluate((el) => ({
    width: el.clientWidth,
    rowHeight: el.closest('.setting-row')!.clientHeight,
  }))
  expect(box.width).toBeLessThan(32)
  expect(box.rowHeight).toBeLessThan(40) // one line, not a wrapped label
})

test('the account-management link matches the Settings buttons', async ({
  page,
}) => {
  await signIn(page)
  await page.goto('/settings')

  const controls = [
    page.getByRole('link', { name: 'Manage accounts' }),
    page.getByRole('button', { name: 'Mark all as read' }),
    page.getByRole('button', { name: 'Debug' }),
  ]
  const appearances = await Promise.all(
    controls.map((control) =>
      control.evaluate((element) => {
        const style = getComputedStyle(element)
        return {
          backgroundColor: style.backgroundColor,
          border: style.border,
          borderRadius: style.borderRadius,
          color: style.color,
          cursor: style.cursor,
          font: style.font,
          padding: style.padding,
          textDecoration: style.textDecoration,
        }
      }),
    ),
  )

  expect(appearances[0]).toEqual(appearances[1])
  expect(appearances[0]).toEqual(appearances[2])
})
