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

  const favs = page.getByRole('button', { name: 'Favorites', exact: true })
  await expect(favs).toHaveAttribute('aria-pressed', 'true')
  await favs.click()
  await expect(names).toHaveText(['Alpha', 'Beta', 'Charlie', 'Other', 'Zulu'])
  await expect(page.locator('.room-separator')).toHaveCount(0)

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
  await expect(names).toHaveText(['Zulu', 'Beta', 'Alpha', 'Charlie'])
  await page.evaluate(() => location.reload())
  await expect(favs).toHaveAttribute('aria-pressed', 'true')
  await expect(names).toHaveText(['Zulu', 'Beta', 'Alpha', 'Charlie'])
})
