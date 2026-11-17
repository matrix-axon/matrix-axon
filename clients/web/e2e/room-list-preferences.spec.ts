import { expect, test } from '@playwright/test'
import { ACCOUNT_ID, ACCOUNT_ID_2, signIn } from './helpers'

test('Favs combines with filters and restores every local preference after reload', async ({
  page,
}) => {
  await signIn(page)
  await page.route(
    (url) => url.pathname === '/v1/rooms',
    async (route) => {
      await route.fulfill({
        json: {
          data: [
            {
              room_id: '!z:hs',
              name: 'Zulu',
              tags: [{ name: 'm.favourite', order: 0.1 }],
            },
            {
              room_id: '!b:hs',
              name: 'Beta',
              tags: [{ name: 'm.favourite', order: 0.8 }],
            },
            { room_id: '!a:hs', name: 'Alpha' },
            { room_id: '!c:hs', name: 'Charlie' },
            { room_id: '!dm:hs', name: 'Direct', is_direct: true },
            { room_id: '!other:hs', name: 'Other', account_id: ACCOUNT_ID_2 },
          ].map((room, index) => ({
            account_id: ACCOUNT_ID,
            account_user_id: '@e2e:hs',
            last_activity_ts: index + 1,
            notification_count: 1,
            highlight_count: 0,
            ...room,
          })),
        },
      })
    },
  )
  await page.goto('/')
  const names = page.locator('.room-row .room-name')
  await expect(names).toHaveCount(6)
  await page
    .getByRole('combobox', { name: 'Sort', exact: true })
    .selectOption('az')
  await page.getByRole('button', { name: 'Groups', exact: true }).click()
  await expect(names).toHaveText(['Zulu', 'Beta', 'Alpha', 'Charlie', 'Other'])

  const favs = page.getByRole('button', { name: 'Favs', exact: true })
  await expect(
    page
      .getByRole('group', { name: 'Filter', exact: true })
      .getByRole('button', { name: 'Favs', exact: true }),
  ).toHaveCount(0)
  const toggleBox = await favs.boundingBox()
  expect(toggleBox).not.toBeNull()
  expect(toggleBox!.x).toBeGreaterThanOrEqual(0)
  expect(toggleBox!.x + toggleBox!.width).toBeLessThanOrEqual(
    page.viewportSize()!.width,
  )
  await expect(favs).toHaveAttribute('aria-pressed', 'true')
  await page
    .getByRole('button', { name: 'Reorder favorites', exact: true })
    .click()
  await favs.click()
  await expect(names).toHaveText(['Alpha', 'Beta', 'Charlie', 'Other', 'Zulu'])
  await expect(page.locator('.room-separator')).toHaveCount(0)
  const link = page.locator('.room-link').first()
  await expect
    .poll(() => link.evaluate((node) => (node as HTMLAnchorElement).draggable))
    .toBe(true)
  expect(await link.getAttribute('draggable')).toBeNull()

  const account = page
    .locator('.room-controls')
    .getByRole('combobox', { name: 'Account', exact: true })
  await account.selectOption(ACCOUNT_ID)
  await page.getByRole('searchbox', { name: 'Filter by name' }).fill('b')
  await expect(names).toHaveText(['Beta'])
  await page.evaluate(() => location.reload())
  await expect(names).toHaveText(['Beta'])
  await expect(favs).toHaveAttribute('aria-pressed', 'false')
  await expect(account).toHaveValue(ACCOUNT_ID)
  await expect(
    page.getByRole('combobox', { name: 'Sort', exact: true }),
  ).toHaveValue('az')
  await expect(
    page.getByRole('searchbox', { name: 'Filter by name' }),
  ).toHaveValue('b')

  await page.getByRole('button', { name: 'Clear room filter' }).click()
  await expect(
    page.getByRole('button', { name: 'Groups', exact: true }),
  ).toHaveClass(/active/)
  await expect(names).toHaveText(['Alpha', 'Beta', 'Charlie', 'Zulu'])
  await favs.click()
  await expect(page.locator('.room-reorder')).toHaveCount(0)
  await expect(names).toHaveText(['Zulu', 'Beta', 'Alpha', 'Charlie'])
  await page.evaluate(() => location.reload())
  await expect(favs).toHaveAttribute('aria-pressed', 'true')
  await expect(names).toHaveText(['Zulu', 'Beta', 'Alpha', 'Charlie'])
})

test('typing in a stale tab preserves another tab’s sort, Favs, and theme preferences', async ({
  page,
  context,
}) => {
  await signIn(page)
  await page.goto('/')
  await expect(page.getByText('E2E Room', { exact: true })).toBeVisible()
  const other = await context.newPage()
  await other.goto('/')
  await expect(other.getByText('E2E Room', { exact: true })).toBeVisible()
  await page
    .getByRole('combobox', { name: 'Sort', exact: true })
    .selectOption('az')
  await page.getByRole('button', { name: 'Favs', exact: true }).click()
  await page.goto('/settings')
  await page.getByRole('radio', { name: 'Dark', exact: true }).check()
  await other.getByRole('searchbox', { name: 'Filter by name' }).fill('E2E')
  // This reload also verifies the pagehide flush inside the save debounce.
  await other.evaluate(() => location.reload())
  await expect(
    other.getByRole('searchbox', { name: 'Filter by name' }),
  ).toHaveValue('E2E')
  await expect(
    other.getByRole('combobox', { name: 'Sort', exact: true }),
  ).toHaveValue('az')
  await expect(
    other.getByRole('button', { name: 'Favs', exact: true }),
  ).toHaveAttribute('aria-pressed', 'false')
  await expect(other.locator('html')).toHaveAttribute('data-theme', 'dark')
  await other.close()
})
