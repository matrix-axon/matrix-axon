import { expect, test, type Page } from '@playwright/test'
import { signIn } from './helpers'

/** What the user does at the authorization server, which no spec can reach. */
async function moveFlow(page: Page, query: string): Promise<void> {
  await page.evaluate(
    (search) => fetch(`/__e2e/browser-sign-in${search}`, { method: 'POST' }),
    query,
  )
}

async function startBrowserSignIn(page: Page): Promise<void> {
  await signIn(page)
  await page.goto('/accounts')
  // The mock keeps one flow for every spec and project that shares it.
  await moveFlow(page, '')
  await page.getByRole('tab', { name: 'Sign in with browser' }).click()
  await page.getByLabel('Matrix user ID').fill('@me:hs')
  await page.getByRole('button', { name: 'Start browser sign-in' }).click()
}

test('the sign-in link opens in a new tab and the page finishes by itself', async ({
  page,
  context,
}) => {
  await context.route('https://auth.example/**', (route) =>
    route.fulfill({ contentType: 'text/html', body: '<h1>Approve</h1>' }),
  )
  await startBrowserSignIn(page)

  const link = page.getByRole('link', { name: 'Open sign-in page' })
  await expect(link).toBeVisible()
  await expect(page.getByText('auth.example', { exact: true })).toBeVisible()
  // The mock's link carries no code, so the page is the only place to read it.
  await expect(page.getByText('WXYZ-1234')).toBeVisible()

  const [approval] = await Promise.all([
    context.waitForEvent('page'),
    link.click(),
  ])
  await approval.waitForLoadState()
  expect(approval.url()).toBe('https://auth.example/link')
  // `noopener`: the authorization page gets no handle on the Axon tab.
  expect(await approval.evaluate(() => window.opener)).toBeNull()
  // The Axon tab is still Axon, and still waiting.
  expect(new URL(page.url()).pathname).toBe('/accounts')
  await approval.close()

  await moveFlow(page, '?stage=done')

  await expect(page.getByText('Signed in.')).toBeVisible()
  await expect(link).toHaveCount(0)
  await expect(page.getByText('WXYZ-1234')).toHaveCount(0)
  // Unlike QR sign-in, it does not leave for the room list.
  expect(new URL(page.url()).pathname).toBe('/accounts')
})

test('a reload during the wait comes back to the same sign-in', async ({
  page,
}) => {
  await startBrowserSignIn(page)
  await expect(page.getByText('WXYZ-1234')).toBeVisible()

  await page.reload()

  await expect(
    page.getByRole('tab', { name: 'Sign in with browser' }),
  ).toHaveAttribute('aria-selected', 'true')
  await expect(page.getByText('WXYZ-1234')).toBeVisible()

  await page.getByRole('button', { name: 'Cancel sign-in' }).click()
  await expect(page.getByText('Browser sign-in cancelled.')).toBeVisible()
  await page.reload()
  await expect(
    page.getByRole('tab', { name: 'Sign in with password' }),
  ).toHaveAttribute('aria-selected', 'true')
})

test('a refusal in the browser is explained and can be retried', async ({
  page,
}) => {
  await startBrowserSignIn(page)
  await expect(page.getByText('WXYZ-1234')).toBeVisible()

  await moveFlow(page, '?stage=failed&error_code=authorization_denied')

  await expect(page.getByText(/declined in the browser/)).toBeVisible()
  await page.getByRole('button', { name: 'Start again' }).click()
  await expect(
    page.getByRole('button', { name: 'Start browser sign-in' }),
  ).toBeVisible()
})
